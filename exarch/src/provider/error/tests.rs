use super::*;
use crate::provider::retry::Wait;
use genai::{ModelIden, adapter::AdapterKind};
use ral_core::{data::datum::Datum, fact::*};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::json;
use std::time::Duration;

fn iden() -> ModelIden {
    ModelIden::new(AdapterKind::Anthropic, "m")
}

fn http(status: StatusCode, body: &str, headers: HeaderMap) -> genai::Error {
    genai::Error::HttpError {
        status,
        canonical_reason: status.canonical_reason().unwrap_or("").to_owned(),
        body: body.to_owned(),
        headers: Box::new(headers),
    }
}

fn classify(error: &genai::Error) -> ProviderError {
    ProviderError::from_genai(error, "configured service", "m")
}

fn stream(error: impl std::error::Error + Send + Sync + 'static) -> genai::Error {
    genai::Error::WebStream {
        model_iden: iden(),
        cause: "derived wrapper".into(),
        error: Box::new(error),
    }
}

#[test]
fn from_genai_classifies_stream_timeouts_as_transient() {
    for (timeout, expected) in [
        (genai::webc::StreamTimeout::Read, StreamTimeoutKind::Read),
        (
            genai::webc::StreamTimeout::Headers,
            StreamTimeoutKind::Headers,
        ),
    ] {
        let error = classify(&stream(timeout));
        let ProviderError::Transient {
            cause: RetryableProviderCause::Service(cause),
            attempts,
        } = error
        else {
            panic!("expected transient")
        };
        assert_eq!(attempts, 1);
        assert_eq!(cause.cause.as_ref(), &ServiceCause::StreamTimeout(expected));
    }
}

#[test]
fn from_genai_unknown_boxed_error_and_non_json_response_stay_terminal() {
    for error in [
        genai::Error::WebStream {
            model_iden: iden(),
            cause: "error sending request for url".into(),
            error: Box::<dyn std::error::Error + Send + Sync>::from("body timeout"),
        },
        genai::Error::WebModelCall {
            model_iden: iden(),
            webc_error: genai::webc::Error::ResponseFailedNotJson {
                content_type: "text/html".into(),
                body: "<html>200 but not JSON</html>".into(),
            },
        },
        genai::Error::ChatReqHasNoMessages { model_iden: iden() },
    ] {
        assert!(matches!(classify(&error), ProviderError::Terminal(_)));
    }
}

#[test]
fn from_genai_429_and_5xx_and_400_preserve_all_existing_response_routes() {
    for status in [
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::BAD_REQUEST,
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, HeaderValue::from_static("7"));
        for error in [
            http(status, "", headers.clone()),
            stream(http(status, "", headers.clone())),
            genai::Error::WebModelCall {
                model_iden: iden(),
                webc_error: genai::webc::Error::ResponseFailedStatus {
                    status,
                    body: String::new(),
                    headers: Box::new(headers.clone()),
                },
            },
            genai::Error::WebAdapterCall {
                adapter_kind: AdapterKind::Anthropic,
                webc_error: genai::webc::Error::ResponseFailedStatus {
                    status,
                    body: String::new(),
                    headers: Box::new(headers.clone()),
                },
            },
        ] {
            let error = classify(&error);
            assert_eq!(error.status(), Some(status.as_u16()));
            match status {
                StatusCode::TOO_MANY_REQUESTS => {
                    let ProviderError::Refused(refusal) = error else {
                        panic!("expected refusal")
                    };
                    assert_eq!(refusal.limit, Limit::Rate);
                    assert!(matches!(refusal.cause, ProviderRefusalCause::Service(_)));
                    assert_eq!(
                        refusal.recovery(),
                        Recovery::InPlace(Wait::patient(Duration::from_secs(7)))
                    );
                }
                StatusCode::SERVICE_UNAVAILABLE => assert!(matches!(
                    error,
                    ProviderError::Transient { attempts: 1, .. }
                )),
                StatusCode::BAD_REQUEST => assert!(matches!(error, ProviderError::Api { .. })),
                _ => panic!("fixture status"),
            }
        }
    }
}

#[test]
fn from_genai_long_retry_after_is_deferred_rate() {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::RETRY_AFTER,
        HeaderValue::from_static("600"),
    );
    let error = classify(&http(StatusCode::TOO_MANY_REQUESTS, "", headers));
    let ProviderError::Refused(refusal) = &error else {
        panic!("expected refusal")
    };
    assert_eq!(refusal.limit, Limit::Rate);
    assert!(matches!(refusal.recovery(), Recovery::Deferred(_)));
    assert!(error.summary().starts_with("rate limited until "));
}

#[test]
fn from_genai_429_reset_reads_full_reply_detail_through_wrapped_http() {
    let body = r#"{"error":{"message":"slow down","detail":"retry-after: 600"}}"#;
    for source in [
        http(StatusCode::TOO_MANY_REQUESTS, body, HeaderMap::new()),
        stream(http(StatusCode::TOO_MANY_REQUESTS, body, HeaderMap::new())),
        stream(stream(http(
            StatusCode::TOO_MANY_REQUESTS,
            body,
            HeaderMap::new(),
        ))),
    ] {
        let error = classify(&source);
        let ProviderError::Refused(refusal) = &error else {
            panic!("expected refusal")
        };
        let resets_at = refusal.received + Duration::from_secs(600);
        assert_eq!(refusal.limit, Limit::Rate);
        assert_eq!(refusal.resets_at, Some(resets_at));
        assert_eq!(refusal.recovery(), Recovery::Deferred(resets_at));
        assert_eq!(error.status(), Some(429));
        assert_eq!(error.service_failure().unwrap().body(), Some(body));
        assert_eq!(
            error.summary(),
            format!("{}: slow down", refusal.headline())
        );
    }
}

#[test]
fn from_genai_usage_limit_named_reset_and_unwaitable_quota_keep_policy() {
    let resets_at = Timestamp::now().as_second() + 5 * 3600;
    let error = classify(&http(
        StatusCode::TOO_MANY_REQUESTS,
        &json!({
            "error": { "type": "usage_limit_reached", "resets_at": resets_at, "plan_type": "plus" }
        })
        .to_string(),
        HeaderMap::new(),
    ));
    let ProviderError::Refused(refusal) = error else {
        panic!("expected refusal")
    };
    assert_eq!(refusal.limit, Limit::Allowance);
    assert_eq!(refusal.resets_at.map(Timestamp::as_second), Some(resets_at));
    assert!(matches!(refusal.recovery(), Recovery::Deferred(_)));
    for body in [
        json!({"error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "You have hit your usage limit."}}),
        json!({"error": {"type": "insufficient_quota", "message": "You exceeded your quota"}}),
    ] {
        let error = classify(&http(
            StatusCode::TOO_MANY_REQUESTS,
            &body.to_string(),
            HeaderMap::new(),
        ));
        assert!(matches!(error, ProviderError::Api { .. }));
        assert_eq!(error.status(), Some(429));
    }
}

#[test]
fn from_genai_json_body_status_and_statusless_sse_remain_responses() {
    for (body, expected) in [
        (json!({"error": {"code": 429}}), "rate limited"),
        (json!({"code": 503}), "server error"),
        (json!({"error": {"code": 400}}), "api error"),
        (json!({"message": "no code"}), "api error"),
    ] {
        for boxed in [false, true] {
            let mut source = genai::Error::ChatResponse {
                model_iden: iden(),
                body: body.clone(),
            };
            if boxed {
                source = stream(source);
            }
            let error = classify(&source);
            assert_eq!(error.kind(), expected);
            assert_eq!(error.body().as_deref(), Some(&body));
        }
    }
    let body = json!({"type":"error", "error": {
        "type":"invalid_request_error", "code":"subscription_sharing_usage_limit_exceeded",
        "message":"The service asks the user to try after the limit resets", "param":null
    }, "sequence_number":2});
    let error = classify(&stream(genai::Error::ChatResponse {
        model_iden: iden(),
        body: body.clone(),
    }));
    assert!(matches!(error, ProviderError::Api { .. }));
    assert_eq!(error.status(), None);
    assert_eq!(error.body().as_deref(), Some(&body));
    assert_eq!(
        error.summary(),
        "api error: The service asks the user to try after the limit resets"
    );
}

#[test]
fn from_genai_json_body_status_matches_classifier_validation() {
    for (code, expected) in [
        (42, None),
        (99, None),
        (100, Some(100)),
        (999, Some(999)),
        (1000, None),
    ] {
        for body in [
            json!({"error": {"code": code, "message": "failure"}}),
            json!({"code": code, "message": "failure"}),
        ] {
            for boxed in [false, true] {
                let mut source = genai::Error::ChatResponse {
                    model_iden: iden(),
                    body: body.clone(),
                };
                if boxed {
                    source = stream(source);
                }
                let Fault::Response { status, .. } = Fault::of(&source) else {
                    panic!("expected response")
                };
                assert_eq!(status.map(|status| status.as_u16()), expected);
                let error = classify(&source);
                assert!(matches!(error, ProviderError::Api { .. }));
                assert_eq!(error.status(), expected);
                assert_eq!(error.body().as_deref(), Some(&body));
                assert_eq!(
                    error.summary(),
                    match expected {
                        Some(status) => format!("api error {status}: failure"),
                        None => "api error: failure".to_owned(),
                    }
                );
            }
        }
    }
}

#[test]
fn from_genai_relay_view_preserves_original_service_reply() {
    let raw = json!({"error":{"message":"unsupported schema keyword","param":"tools","type":"invalid_request_error"}});
    let body = json!({"error":{"code":"400","message":"Provider returned error", "metadata":{"raw":raw.to_string(),"provider_name":"ModelRun"}}});
    let text = body.to_string();
    let error = classify(&http(StatusCode::BAD_REQUEST, &text, HeaderMap::new()));
    assert_eq!(error.summary(), "api error 400: unsupported schema keyword");
    assert_eq!(error.service_failure().unwrap().body(), Some(text.as_str()));
    let view = error.body().unwrap();
    let obj = error_object(&view).unwrap();
    assert_eq!(obj["param"], "tools");
    assert_eq!(obj["provider"], "ModelRun");
}

#[test]
fn from_genai_relayed_vllm_anthropic_and_typed_overflow_keep_counts() {
    let raw = json!({"object":"error", "message":"The input (250015 tokens) is longer than the model's context length (250000 tokens).", "type":"context_length_exceeded"});
    let body = json!({"error":{"code":"400","message":"Provider returned error","metadata":{"raw":raw.to_string(),"provider_name":"InferenceNet"}}});
    let error = classify(&http(
        StatusCode::BAD_REQUEST,
        &body.to_string(),
        HeaderMap::new(),
    ));
    assert!(
        matches!(&error, ProviderError::Overflow { sent: Some(250_015), limit: Some(250_000), provider: Some(provider), .. } if provider == "InferenceNet")
    );
    assert_eq!(
        error.summary(),
        "context too long for InferenceNet: 250015 tokens sent, 250000 allowed"
    );
    for (body, sent, limit) in [
        (
            json!({"error":{"message":"prompt is too long: 210000 tokens > 200000 maximum"}}),
            Some(210_000),
            Some(200_000),
        ),
        (
            json!({"error":{"type":"context_length_exceeded"}}),
            None,
            None,
        ),
        (
            json!({"message":"input token count (31) exceeds the maximum number of tokens allowed (30)"}),
            Some(31),
            Some(30),
        ),
        (
            json!({"message":"maximum context length is 40 tokens; requested about 41 tokens"}),
            Some(41),
            Some(40),
        ),
    ] {
        let error = classify(&http(
            StatusCode::BAD_REQUEST,
            &body.to_string(),
            HeaderMap::new(),
        ));
        assert!(
            matches!(error, ProviderError::Overflow { sent: actual_sent, limit: actual_limit, .. } if actual_sent == sent && actual_limit == limit)
        );
    }
    assert!(matches!(
        classify(&http(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"unsupported schema keyword"}}"#,
            HeaderMap::new()
        )),
        ProviderError::Api { .. }
    ));
}

#[test]
fn from_genai_non_json_http_body_is_external_text_fallback() {
    let error = classify(&http(
        StatusCode::BAD_REQUEST,
        "prompt is too long: 21 tokens > 20 maximum",
        HeaderMap::new(),
    ));
    assert!(matches!(
        error,
        ProviderError::Overflow {
            sent: Some(21),
            limit: Some(20),
            ..
        }
    ));
    assert_eq!(
        error.service_failure().unwrap().body(),
        Some("prompt is too long: 21 tokens > 20 maximum")
    );
}

#[test]
fn summary_reads_body_message_not_wrapper_display() {
    let body =
        json!({"type":"error","error":{"message":"Weekly usage limit reached. Resets in 4 days."}});
    let error = classify(&http(
        StatusCode::TOO_MANY_REQUESTS,
        &body.to_string(),
        HeaderMap::new(),
    ));
    assert_eq!(
        error.summary(),
        "rate limited: Weekly usage limit reached. Resets in 4 days."
    );
    let error = classify(&http(StatusCode::TOO_MANY_REQUESTS, "", HeaderMap::new()));
    assert_eq!(error.summary(), "rate limited");
}

#[test]
fn refusal_held_is_unsent_and_instants_preserve_signed_subseconds() {
    let received = Timestamp::from_nanosecond(-1).unwrap();
    let resets_at = Timestamp::from_nanosecond(1_000_000_001).unwrap();
    for (limit, expected_limit) in [
        (Limit::Rate, ProviderLimit::Rate),
        (Limit::Allowance, ProviderLimit::Allowance),
    ] {
        let refusal = Refusal::held(limit, Some(resets_at), received, "service", "model");
        let projected = refusal.failure();
        assert_eq!(projected.limit, expected_limit);
        assert_eq!(
            projected.received,
            ProviderInstant {
                seconds: received.as_second(),
                subsec_nanoseconds: received.subsec_nanosecond()
            }
        );
        assert_eq!(
            projected.resets_at,
            Some(ProviderInstant {
                seconds: resets_at.as_second(),
                subsec_nanoseconds: resets_at.subsec_nanosecond()
            })
        );
        assert_eq!(
            projected.cause,
            ProviderRefusalCause::Held(ProviderRefusalCauseHeld {
                service: "service".into(),
                model: "model".into()
            })
        );
        assert_eq!(
            ProviderRefusal::decode(&projected.clone().encode()).unwrap(),
            projected
        );
    }
}

#[test]
fn provider_projection_every_owner_arm_and_lossless_counts() {
    let service = projection::genai_failure(&genai::Error::Internal("intrinsic".into()), "service");
    let recording = RecordingFailure::CellPoisoned(RecordingFailureCellPoisoned {
        agent: Count::from_u64(u64::MAX),
    });
    let credential = CredentialFailure {
        account: "account".into(),
        service: CredentialService::Codex,
        cause: CredentialCause::OwnerStopped,
    };
    let cases = [
        (
            ProviderError::Unavailable(recording.clone()),
            ProviderLeaf::Unavailable(recording),
        ),
        (
            ProviderError::Cancelled(CancelSite::BeforeRequest),
            ProviderLeaf::Cancelled(ProviderCancelSite::BeforeRequest),
        ),
        (
            ProviderError::Cancelled(CancelSite::MidStream),
            ProviderLeaf::Cancelled(ProviderCancelSite::MidStream),
        ),
        (
            ProviderError::Cancelled(CancelSite::Backoff),
            ProviderLeaf::Cancelled(ProviderCancelSite::Backoff),
        ),
        (
            ProviderError::Transient {
                cause: RetryableProviderCause::Service(service.clone()),
                attempts: u32::MAX,
            },
            ProviderLeaf::Transient(ProviderLeafTransient {
                cause: RetryableProviderCause::Service(service.clone()),
                attempts: Count::from_u64(u64::from(u32::MAX)),
            }),
        ),
        (
            ProviderError::stream_ended_without_end("service", "model"),
            ProviderLeaf::Transient(ProviderLeafTransient {
                cause: RetryableProviderCause::StreamEndedWithoutEnd(
                    RetryableProviderCauseStreamEndedWithoutEnd {
                        service: "service".into(),
                        model: "model".into(),
                    },
                ),
                attempts: Count::from_u64(1),
            }),
        ),
        (
            ProviderError::Api {
                model: "model".into(),
                cause: service.clone(),
            },
            ProviderLeaf::Api(ProviderLeafApi {
                model: "model".into(),
                cause: service.clone(),
            }),
        ),
        (
            ProviderError::Overflow {
                model: "model".into(),
                provider: Some("relay".into()),
                sent: Some(u64::MAX),
                limit: Some(i64::MAX as u64 + 1),
                listed: Some(u64::MAX),
                cause: Box::new(service.clone()),
            },
            ProviderLeaf::Overflow(ProviderLeafOverflow {
                model: "model".into(),
                provider: Some("relay".into()),
                sent: Some(Count::from_u64(u64::MAX)),
                limit: Some(Count::from_u64(i64::MAX as u64 + 1)),
                listed: Some(Count::from_u64(u64::MAX)),
                cause: service.clone(),
            }),
        ),
        (
            ProviderError::Terminal(service.clone()),
            ProviderLeaf::Terminal(service),
        ),
        (
            ProviderError::Credential(credential.clone()),
            ProviderLeaf::Credential(credential),
        ),
        (
            ProviderError::Truncated {
                cause: Box::new(CutShort::OutputCap {
                    stop_reason: "raw stop".into(),
                }),
            },
            ProviderLeaf::OutputCap(ProviderLeafOutputCap {
                stop_reason: "raw stop".into(),
            }),
        ),
    ];
    for (error, leaf) in cases {
        let projected = error.failure();
        assert_eq!(
            projected,
            ProviderFailure {
                context: Vec::new(),
                leaf
            }
        );
        assert_eq!(
            ProviderFailure::decode(&projected.clone().encode()).unwrap(),
            projected
        );
    }
    let refusal = Refusal::for_test(Limit::Rate, None);
    let expected = refusal.failure();
    assert_eq!(
        ProviderError::Refused(refusal).failure().leaf,
        ProviderLeaf::Refused(expected)
    );
}

#[test]
fn provider_truncation_spine_has_no_depth_cap() {
    let mut error = ProviderError::stream_ended_without_end("service", "model");
    for _ in 0..256 {
        error = ProviderError::Truncated {
            cause: Box::new(CutShort::Stalled(error)),
        };
    }
    assert!(error.stall_cause().is_some());
    let projected = error.failure();
    assert_eq!(projected.context, vec![ProviderContext::Truncated; 256]);
    assert!(matches!(projected.leaf, ProviderLeaf::Transient(_)));
    assert_eq!(
        ProviderFailure::decode(&projected.clone().encode()).unwrap(),
        projected
    );
}

#[test]
fn body_detail_preserves_ui_only_snippet_behavior() {
    for (body, expected) in [
        (
            r#"{"error":{"message":"invalid api key"}}"#,
            "invalid api key",
        ),
        (r#"{"message":"model not found"}"#, "model not found"),
        (
            "<!DOCTYPE html><html>reply</html>",
            "the endpoint returned a web page, not an error message",
        ),
        (
            "\nupstream connect error\nmore detail",
            "upstream connect error",
        ),
        ("", "the endpoint sent no detail"),
        ("  \n  ", "the endpoint sent no detail"),
    ] {
        assert_eq!(body_detail(body), expected);
    }
    let long = "x".repeat(ServiceFailure::BODY_DETAIL_CAP + 50);
    let expected = format!("{}…", "x".repeat(ServiceFailure::BODY_DETAIL_CAP));
    assert_eq!(body_detail(&long), expected);
    assert_eq!(
        body_detail(&json!({"error":{"message":long}}).to_string()),
        expected
    );
}
