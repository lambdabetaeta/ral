use super::*;
use ral_core::{data::datum::Datum, fact::*};
use reqwest::{
    StatusCode,
    header::{HeaderMap, HeaderValue},
};
use serde_json::json;
use std::{fmt, io};

fn iden() -> genai::ModelIden {
    genai::ModelIden::new(AdapterKind::Custom(37), "model")
}
fn expected_model() -> ProviderModel {
    ProviderModel {
        adapter: ProviderAdapter::Custom(37),
        name: "model".into(),
    }
}
fn json_error() -> serde_json::Error {
    serde_json::from_str::<serde_json::Value>("{]").unwrap_err()
}

fn check(error: &genai::Error, expected: GenaiFailure) {
    let failure = genai_failure(error, "configured service");
    assert_eq!(failure.service, "configured service");
    assert_eq!(failure.operation, ServiceOperation::Chat);
    assert_eq!(failure.context, Vec::<ServiceContext>::new());
    assert_eq!(failure.cause.as_ref(), &ServiceCause::Genai(expected));
    assert_eq!(
        ServiceFailure::decode(&failure.clone().encode()).unwrap(),
        failure
    );
}

#[test]
fn genai_0_7_3_every_leaf_and_intrinsic_field() {
    check(
        &genai::Error::ChatReqHasNoMessages { model_iden: iden() },
        GenaiFailure::ChatReqHasNoMessages(expected_model()),
    );
    check(
        &genai::Error::NoChatResponse { model_iden: iden() },
        GenaiFailure::NoChatResponse(expected_model()),
    );
    check(
        &genai::Error::RequiresApiKey { model_iden: iden() },
        GenaiFailure::RequiresApiKey(expected_model()),
    );
    check(
        &genai::Error::NoAuthResolver { model_iden: iden() },
        GenaiFailure::NoAuthResolver(expected_model()),
    );
    check(
        &genai::Error::NoAuthData { model_iden: iden() },
        GenaiFailure::NoAuthData(expected_model()),
    );
    check(
        &genai::Error::LastChatMessageIsNotUser {
            model_iden: iden(),
            actual_role: ChatRole::Tool,
        },
        GenaiFailure::LastChatMessageIsNotUser(GenaiFailureLastChatMessageIsNotUser {
            model: expected_model(),
            actual_role: ProviderMessageRole::Tool,
        }),
    );
    check(
        &genai::Error::MessageRoleNotSupported {
            model_iden: iden(),
            role: ChatRole::Tool,
        },
        GenaiFailure::MessageRoleNotSupported(GenaiFailureMessageRoleNotSupported {
            model: expected_model(),
            role: ProviderMessageRole::Tool,
        }),
    );
    check(
        &genai::Error::MessageContentTypeNotSupported {
            model_iden: iden(),
            cause: "intrinsic content",
        },
        GenaiFailure::MessageContentTypeNotSupported(GenaiFailureMessageContentTypeNotSupported {
            model: expected_model(),
            detail: "intrinsic content".into(),
        }),
    );
    check(
        &genai::Error::JsonModeWithoutInstruction,
        GenaiFailure::JsonModeWithoutInstruction,
    );
    check(
        &genai::Error::VerbosityParsing {
            actual: "actual input".into(),
        },
        GenaiFailure::VerbosityParsing(GenaiFailureVerbosityParsing {
            actual: "actual input".into(),
        }),
    );
    check(
        &genai::Error::ReasoningParsingError {
            actual: "actual input".into(),
        },
        GenaiFailure::ReasoningParsing(GenaiFailureReasoningParsing {
            actual: "actual input".into(),
        }),
    );
    check(
        &genai::Error::ServiceTierParsing {
            actual: "actual input".into(),
        },
        GenaiFailure::ServiceTierParsing(GenaiFailureServiceTierParsing {
            actual: "actual input".into(),
        }),
    );
    check(
        &genai::Error::PromptCacheRetentionParsing {
            actual: "actual input".into(),
        },
        GenaiFailure::PromptCacheRetentionParsing(GenaiFailurePromptCacheRetentionParsing {
            actual: "actual input".into(),
        }),
    );
    check(
        &genai::Error::InvalidJsonResponseElement {
            info: "intrinsic info",
        },
        GenaiFailure::InvalidJsonResponseElement(GenaiFailureInvalidJsonResponseElement {
            detail: "intrinsic info".into(),
        }),
    );
    check(
        &genai::Error::ModelMapperFailed {
            model_iden: iden(),
            cause: genai::resolver::Error::ResolverAuthDataNotSingleValue,
        },
        GenaiFailure::ModelMapperFailed(GenaiFailureModelMapperFailed {
            model: expected_model(),
            cause: ResolverFailure::AuthDataNotSingleValue,
        }),
    );
    let response = json!({"large": u64::MAX, "error": {"message": "reply"}});
    check(
        &genai::Error::ChatResponseGeneration {
            model_iden: iden(),
            request_payload: Box::new(json!({"authorization": "NEVER_CAPTURE_REQUEST"})),
            response_body: Box::new(response.clone()),
            cause: "intrinsic generation".into(),
        },
        GenaiFailure::ChatResponseGeneration(GenaiFailureChatResponseGeneration {
            model: expected_model(),
            response_body: response.to_string(),
            detail: "intrinsic generation".into(),
        }),
    );
    check(
        &genai::Error::ChatResponse {
            model_iden: iden(),
            body: response.clone(),
        },
        GenaiFailure::ChatResponse(GenaiFailureChatResponse {
            model: expected_model(),
            body: response.to_string(),
        }),
    );
    check(
        &genai::Error::StreamParse {
            model_iden: iden(),
            serde_error: json_error(),
        },
        GenaiFailure::StreamParse(GenaiFailureStreamParse {
            model: expected_model(),
            cause: foreign_json(&json_error()),
        }),
    );
    check(
        &genai::Error::HttpError {
            status: StatusCode::BAD_REQUEST,
            canonical_reason: "external reason".into(),
            body: "external body".into(),
            headers: Box::new(headers()),
        },
        GenaiFailure::HttpError(GenaiFailureHttpError {
            status: 400,
            canonical_reason: "external reason".into(),
            body: "external body".into(),
            headers: expected_headers(),
        }),
    );
    check(
        &genai::Error::Resolver {
            model_iden: iden(),
            resolver_error: genai::resolver::Error::Custom("foreign resolver".into()),
        },
        GenaiFailure::Resolver(GenaiFailureResolver {
            model: expected_model(),
            cause: ResolverFailure::Custom(ResolverFailureCustom {
                detail: "foreign resolver".into(),
            }),
        }),
    );
    check(
        &genai::Error::AdapterNotSupported {
            adapter_kind: AdapterKind::Custom(37),
            feature: "feature".into(),
        },
        GenaiFailure::AdapterNotSupported(GenaiFailureAdapterNotSupported {
            adapter: ProviderAdapter::Custom(37),
            feature: "feature".into(),
        }),
    );
    check(
        &genai::Error::CacheBreakpointNoEligibleContent {
            model_iden: iden(),
            scope: "scope",
        },
        GenaiFailure::CacheBreakpointNoEligibleContent(
            GenaiFailureCacheBreakpointNoEligibleContent {
                model: expected_model(),
                scope: "scope".into(),
            },
        ),
    );
    check(
        &genai::Error::AdapterKindMismatch {
            bound: AdapterKind::OpenAI,
            requested: AdapterKind::Anthropic,
            model: "model".into(),
        },
        GenaiFailure::AdapterKindMismatch(GenaiFailureAdapterKindMismatch {
            bound: ProviderAdapter::OpenAi,
            requested: ProviderAdapter::Anthropic,
            model: "model".into(),
        }),
    );
    check(
        &genai::Error::Internal("intrinsic https://configured.endpoint/v1?account=selected".into()),
        GenaiFailure::Internal(GenaiFailureInternal {
            detail: "intrinsic https://configured.endpoint/v1?account=selected".into(),
        }),
    );
    check(
        &genai::Error::ClientBuildFail {
            cause: "intrinsic client".into(),
        },
        GenaiFailure::ClientBuildFail(GenaiFailureClientBuildFail {
            detail: "intrinsic client".into(),
        }),
    );
    check(
        &genai::Error::JsonValueExt(value_ext::JsonValueExtError::PropertyNotFound(
            "field".into(),
        )),
        GenaiFailure::JsonValueExt(ValueExtFailure::PropertyNotFound(
            ValueExtFailurePropertyNotFound {
                name: "field".into(),
            },
        )),
    );
    check(
        &genai::Error::SerdeJson(json_error()),
        GenaiFailure::SerdeJson(foreign_json(&json_error())),
    );
}

fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.append("z-last", HeaderValue::from_static("last"));
    headers.append("a-repeat", HeaderValue::from_static("first"));
    headers.append("a-repeat", HeaderValue::from_bytes(&[0xff, b'x']).unwrap());
    headers
}
fn expected_headers() -> Vec<ProviderResponseHeader> {
    vec![
        ProviderResponseHeader {
            name: "a-repeat".into(),
            value: b"first".as_slice().into(),
        },
        ProviderResponseHeader {
            name: "a-repeat".into(),
            value: vec![0xff, b'x'].into(),
        },
        ProviderResponseHeader {
            name: "z-last".into(),
            value: b"last".as_slice().into(),
        },
    ]
}

#[derive(Debug)]
struct ForeignWrapper {
    detail: &'static str,
    source: Option<Box<dyn Error + Send + Sync>>,
}
impl fmt::Display for ForeignWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail)
    }
}
impl Error for ForeignWrapper {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|e| e as _)
    }
}
fn wrapper(detail: &'static str, source: impl Error + Send + Sync + 'static) -> ForeignWrapper {
    ForeignWrapper {
        detail,
        source: Some(Box::new(source)),
    }
}
fn stream(source: impl Error + Send + Sync + 'static) -> genai::Error {
    genai::Error::WebStream {
        model_iden: iden(),
        cause: "DERIVED_WRAPPER_DISPLAY_MUST_NOT_SURVIVE".into(),
        error: Box::new(source),
    }
}

#[test]
fn service_spine_outermost_first_without_derived_wrapper_cause() {
    let error = stream(wrapper(
        "outer foreign",
        stream(genai::Error::WebModelCall {
            model_iden: genai::ModelIden::new(AdapterKind::Anthropic, "inner model"),
            webc_error: genai::webc::Error::ResponseFailedNotJson {
                content_type: "text/html".into(),
                body: "reply".into(),
            },
        }),
    ));
    let failure = genai_failure(&error, "configured service");
    assert_eq!(
        failure.context,
        vec![
            ServiceContext::GenaiWebStream(expected_model()),
            ServiceContext::UnknownWebStreamSource(ServiceContextUnknownWebStreamSource {
                detail: "outer foreign".into()
            }),
            ServiceContext::GenaiWebStream(expected_model()),
            ServiceContext::GenaiWebModelCall(ProviderModel {
                adapter: ProviderAdapter::Anthropic,
                name: "inner model".into()
            }),
        ]
    );
    assert!(!failure.describe().contains("DERIVED_WRAPPER_DISPLAY"));
    let adapter_error = genai::Error::WebAdapterCall {
        adapter_kind: AdapterKind::OpenAIResp,
        webc_error: genai::webc::Error::ResponseFailedInvalidJson {
            body: "reply".into(),
            cause: "intrinsic".into(),
        },
    };
    assert_eq!(
        genai_failure(&adapter_error, "service").context,
        vec![ServiceContext::GenaiWebAdapterCall(
            ProviderAdapter::OpenAiResp
        )]
    );
}

#[test]
fn boxed_web_stream_all_known_source_types_before_unknown() {
    let invalid = vec![b'a', 0xff];
    let utf8_error = std::str::from_utf8(&invalid).unwrap_err();
    let expected_utf8 = Utf8Failure {
        valid_up_to: Count::from_u64(1),
        error_len: Some(Count::from_u64(1)),
    };
    let reqwest_error = reqwest::Client::new().get("http://[").build().unwrap_err();
    let reqwest_expected = ServiceCause::Reqwest(reqwest_failure(&reqwest_error));
    let cases: Vec<(Box<dyn Error + Send + Sync>, ServiceCause)> = vec![
        (
            Box::new(genai::Error::Internal("intrinsic".into())),
            ServiceCause::Genai(GenaiFailure::Internal(GenaiFailureInternal {
                detail: "intrinsic".into(),
            })),
        ),
        (
            Box::new(genai::webc::StreamTimeout::Headers),
            ServiceCause::StreamTimeout(StreamTimeoutKind::Headers),
        ),
        (
            Box::new(genai::webc::StreamTimeout::Read),
            ServiceCause::StreamTimeout(StreamTimeoutKind::Read),
        ),
        (Box::new(reqwest_error), reqwest_expected),
        (
            Box::new(utf8_error),
            ServiceCause::Utf8(expected_utf8.clone()),
        ),
        (
            Box::new(String::from_utf8(invalid).unwrap_err()),
            ServiceCause::Utf8(expected_utf8),
        ),
        (
            Box::new(genai::webc::Error::ResponseFailedNotJson {
                content_type: "text/plain".into(),
                body: "reply".into(),
            }),
            ServiceCause::Webc(WebcFailure::ResponseFailedNotJson(
                WebcFailureResponseFailedNotJson {
                    content_type: "text/plain".into(),
                    body: "reply".into(),
                },
            )),
        ),
        (
            Box::new(io::Error::from(io::ErrorKind::PermissionDenied)),
            ServiceCause::Io(ral_core::io::failure(&io::Error::from(
                io::ErrorKind::PermissionDenied,
            ))),
        ),
        (
            Box::new(url::ParseError::EmptyHost),
            ServiceCause::Url(ProviderUrlFailure::EmptyHost),
        ),
        (
            Box::new(value_ext::JsonValueExtError::Custom("foreign".into())),
            ServiceCause::ValueExt(ValueExtFailure::Custom(ValueExtFailureCustom {
                detail: "foreign".into(),
            })),
        ),
        (
            Box::new(json_error()),
            ServiceCause::Json(foreign_json(&json_error())),
        ),
        (
            Box::new(ForeignWrapper {
                detail: "terminal foreign",
                source: None,
            }),
            ServiceCause::UnknownWebStreamSource(ServiceCauseUnknownWebStreamSource {
                detail: "terminal foreign".into(),
            }),
        ),
    ];
    for (source, expected) in cases {
        let error = genai::Error::WebStream {
            model_iden: iden(),
            cause: "derived".into(),
            error: source,
        };
        let failure = genai_failure(&error, "service");
        assert_eq!(
            failure.context,
            vec![ServiceContext::GenaiWebStream(expected_model())]
        );
        assert_eq!(failure.cause.as_ref(), &expected);
    }
    let error = stream(wrapper(
        "outer",
        wrapper("inner", url::ParseError::InvalidPort),
    ));
    let failure = genai_failure(&error, "service");
    assert_eq!(
        failure.context[1..],
        [
            ServiceContext::UnknownWebStreamSource(ServiceContextUnknownWebStreamSource {
                detail: "outer".into()
            }),
            ServiceContext::UnknownWebStreamSource(ServiceContextUnknownWebStreamSource {
                detail: "inner".into()
            }),
        ]
    );
    assert_eq!(
        failure.cause.as_ref(),
        &ServiceCause::Url(ProviderUrlFailure::InvalidPort)
    );
}

#[test]
fn reqwest_sources_known_types_and_immediate_to_deepest_order() {
    let invalid = vec![b'a', 0xff];
    let reqwest = reqwest::Client::new().get("http://[").build().unwrap_err();
    let chain = wrapper("outer foreign", wrapper("inner foreign", reqwest));
    let projected = reqwest_sources(Some(&chain));
    assert_eq!(
        projected[0],
        ReqwestSource::Foreign(ReqwestSourceForeign {
            detail: "outer foreign".into()
        })
    );
    assert_eq!(
        projected[1],
        ReqwestSource::Foreign(ReqwestSourceForeign {
            detail: "inner foreign".into()
        })
    );
    assert!(matches!(
        &projected[2],
        ReqwestSource::Reqwest(ReqwestSummary {
            class: ReqwestClass::Builder,
            ..
        })
    ));
    assert!(matches!(
        &projected[3],
        ReqwestSource::Url(ProviderUrlFailure::InvalidIpv6Address)
    ));
    let error = json_error();
    assert_eq!(
        reqwest_sources(Some(&error)),
        vec![ReqwestSource::Json(foreign_json(&error))]
    );
    let error = io::Error::from(io::ErrorKind::ConnectionReset);
    assert_eq!(
        reqwest_sources(Some(&error)),
        vec![ReqwestSource::Io(ral_core::io::failure(&error))]
    );
    for error in [
        Box::new(std::str::from_utf8(&invalid).unwrap_err()) as Box<dyn Error>,
        Box::new(String::from_utf8(invalid).unwrap_err()),
    ] {
        assert_eq!(
            reqwest_sources(Some(error.as_ref())),
            vec![ReqwestSource::Utf8(Utf8Failure {
                valid_up_to: Count::from_u64(1),
                error_len: Some(Count::from_u64(1))
            })]
        );
    }
}

#[test]
fn reqwest_sources_do_not_duplicate_consumed_io_context_spine() {
    let cause = IoCause::new(IoLeaf::Os(OsFailure {
        kind: IoKind::PermissionDenied,
        raw: Some(13),
        detail: None,
    }))
    .at(IoContext::Owned(OwnIoContext {
        act: OwnAct::Read,
        at: ral_core::text::quote_native(std::ffi::OsStr::new("owned")),
        to: None,
    }));
    let error = io::Error::other(cause.clone());
    assert_eq!(
        reqwest_sources(Some(&error)),
        vec![ReqwestSource::Io(cause)]
    );
}

fn server(
    reply: &'static str,
    after_headers: std::time::Duration,
) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            if socket.read(&mut byte).unwrap_or(0) == 0 {
                return;
            }
            bytes.push(byte[0]);
        }
        let _ = socket.write_all(reply.as_bytes());
        std::thread::sleep(after_headers);
    });
    (format!("http://{address}/endpoint"), thread)
}

#[tokio::test]
async fn reqwest_0_13_5_public_classes_url_and_foreign_source_metadata() {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let builder = client.get("http://[").build().unwrap_err();
    let projected = reqwest_failure(&builder);
    assert_eq!(
        projected.summary,
        ReqwestSummary {
            class: ReqwestClass::Builder,
            timeout: false,
            connect: false,
            dns: false,
            url: None
        }
    );
    assert_eq!(
        projected.sources,
        vec![ReqwestSource::Url(ProviderUrlFailure::InvalidIpv6Address)]
    );

    let (url, thread) = server(
        "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        std::time::Duration::ZERO,
    );
    let status = client
        .get(&url)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap_err();
    thread.join().unwrap();
    assert_eq!(
        reqwest_failure(&status),
        ReqwestFailure {
            summary: ReqwestSummary {
                class: ReqwestClass::Status(429),
                timeout: false,
                connect: false,
                dns: false,
                url: Some(url)
            },
            sources: Vec::new(),
        }
    );
    let owner = super::super::ProviderError::from_genai(
        &genai::Error::WebModelCall {
            model_iden: iden(),
            webc_error: genai::webc::Error::Reqwest(status),
        },
        "service",
        "model",
    );
    assert!(
        matches!(owner, super::super::ProviderError::Terminal(_)),
        "reqwest status must not be rerouted as a received response"
    );

    let (url, thread) = server(
        "HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        std::time::Duration::ZERO,
    );
    let redirect = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            attempt.error("foreign redirect refusal")
        }))
        .build()
        .unwrap()
        .get(&url)
        .send()
        .await
        .unwrap_err();
    thread.join().unwrap();
    let projected = reqwest_failure(&redirect);
    assert_eq!(projected.summary.class, ReqwestClass::Redirect);
    assert!(!projected.summary.timeout && !projected.summary.connect && !projected.summary.dns);
    assert_eq!(projected.summary.url.as_deref(), Some(url.as_str()));
    assert!(
        matches!(&projected.sources[0], ReqwestSource::Foreign(ReqwestSourceForeign { detail }) if detail == "foreign redirect refusal")
    );

    let (url, thread) = server(
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{]",
        std::time::Duration::ZERO,
    );
    let decode = client
        .get(&url)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap_err();
    thread.join().unwrap();
    let projected = reqwest_failure(&decode);
    assert_eq!(projected.summary.class, ReqwestClass::Decode);
    assert!(!projected.summary.timeout && !projected.summary.connect && !projected.summary.dns);
    assert_eq!(projected.summary.url.as_deref(), Some(url.as_str()));
    assert!(
        matches!(&projected.sources[0], ReqwestSource::Json(ForeignJsonFailure { category: ForeignJsonCategory::Syntax, line, column, io_kind: None, .. }) if line.to_u64() == 1 && column.to_u64() == 2)
    );

    let (url, thread) = server(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        std::time::Duration::ZERO,
    );
    let upgrade = client
        .get(url)
        .send()
        .await
        .unwrap()
        .upgrade()
        .await
        .unwrap_err();
    thread.join().unwrap();
    assert_eq!(
        reqwest_failure(&upgrade).summary.class,
        ReqwestClass::Upgrade
    );
    assert!(
        !reqwest_summary(&upgrade).timeout
            && !reqwest_summary(&upgrade).connect
            && !reqwest_summary(&upgrade).dns
    );
    assert_ne!(
        reqwest_failure(&upgrade).sources,
        Vec::<ReqwestSource>::new()
    );
}

#[derive(Debug)]
struct FailingDns(io::ErrorKind);
impl reqwest::dns::Resolve for FailingDns {
    fn resolve(&self, _: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let kind = self.0;
        Box::pin(
            async move { Err(Box::new(io::Error::from(kind)) as Box<dyn Error + Send + Sync>) },
        )
    }
}

#[tokio::test]
async fn reqwest_flags_are_independent_request_connect_dns_timeout_observations() {
    for (kind, timeout) in [
        (io::ErrorKind::TimedOut, true),
        (io::ErrorKind::NotFound, false),
    ] {
        let error = reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(std::sync::Arc::new(FailingDns(kind)))
            .build()
            .unwrap()
            .get("http://fixture.invalid/endpoint")
            .send()
            .await
            .unwrap_err();
        let failure = reqwest_failure(&error);
        assert_eq!(
            failure.summary,
            ReqwestSummary {
                class: ReqwestClass::Request,
                timeout,
                connect: true,
                dns: true,
                url: Some("http://fixture.invalid/endpoint".into()),
            }
        );
        assert!(failure.sources.iter().any(|source| matches!(source, ReqwestSource::Io(cause) if matches!(cause.leaf.as_ref(), IoLeaf::Os(OsFailure { kind: actual, .. }) if *actual == IoKind::from(kind)))));
        let owner = super::super::ProviderError::from_genai(
            &genai::Error::WebModelCall {
                model_iden: iden(),
                webc_error: genai::webc::Error::Reqwest(error),
            },
            "service",
            "model",
        );
        assert!(matches!(
            owner,
            super::super::ProviderError::Transient { attempts: 1, .. }
        ));
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/endpoint", listener.local_addr().unwrap());
    drop(listener);
    let error = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(&url)
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        reqwest_failure(&error).summary,
        ReqwestSummary {
            class: ReqwestClass::Request,
            timeout: false,
            connect: true,
            dns: false,
            url: Some(url)
        }
    );
}

#[tokio::test]
async fn reqwest_decode_and_body_timeout_keep_nested_summary_and_all_flags() {
    let (url, thread) = server(
        "HTTP/1.1 200 OK\r\nContent-Length: 99\r\nConnection: close\r\n\r\nx",
        std::time::Duration::from_secs(1),
    );
    let error = reqwest::Client::builder()
        .no_proxy()
        .read_timeout(std::time::Duration::from_millis(50))
        .build()
        .unwrap()
        .get(&url)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap_err();
    thread.join().unwrap();
    let failure = reqwest_failure(&error);
    assert_eq!(
        failure.summary,
        ReqwestSummary {
            class: ReqwestClass::Decode,
            timeout: true,
            connect: false,
            dns: false,
            url: Some(url)
        }
    );
    let mut source = error.source();
    let body = loop {
        let current = source.expect("reqwest's read-timeout Body source");
        if let Some(error) = current
            .downcast_ref::<reqwest::Error>()
            .filter(|error| error.is_body())
        {
            break error;
        }
        source = current.source();
    };
    assert_eq!(
        reqwest_summary(body),
        ReqwestSummary {
            class: ReqwestClass::Body,
            timeout: true,
            connect: false,
            dns: false,
            url: None
        }
    );
    assert!(failure.sources.iter().any(|source| matches!(
        source,
        ReqwestSource::Reqwest(ReqwestSummary {
            class: ReqwestClass::Body,
            timeout: true,
            connect: false,
            dns: false,
            ..
        })
    )));
    assert_eq!(
        reqwest_failure(body).sources,
        reqwest_sources(body.source())
    );
}
