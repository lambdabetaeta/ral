//! Exact foreign boundaries; no general Error/Display-to-fact conversion.

use genai::{
    adapter::AdapterKind,
    chat::{ChatRole, StopReason},
};
use ral_core::fact::{
    Count, ForeignJsonCategory, ForeignJsonFailure, GenaiFailure, GenaiFailureAdapterKindMismatch,
    GenaiFailureAdapterNotSupported, GenaiFailureCacheBreakpointNoEligibleContent,
    GenaiFailureChatResponse, GenaiFailureChatResponseGeneration, GenaiFailureClientBuildFail,
    GenaiFailureHttpError, GenaiFailureInternal, GenaiFailureInvalidJsonResponseElement,
    GenaiFailureLastChatMessageIsNotUser, GenaiFailureMessageContentTypeNotSupported,
    GenaiFailureMessageRoleNotSupported, GenaiFailureModelMapperFailed,
    GenaiFailurePromptCacheRetentionParsing, GenaiFailureReasoningParsing, GenaiFailureResolver,
    GenaiFailureServiceTierParsing, GenaiFailureStreamParse, GenaiFailureVerbosityParsing, IoLeaf,
    ProviderAdapter, ProviderMessageRole, ProviderModel, ProviderResponseHeader, ProviderStop,
    ProviderUrlFailure, ProviderUrlFailureUnclassified, ReqwestClass, ReqwestClassUnclassified,
    ReqwestFailure, ReqwestSource, ReqwestSourceForeign, ReqwestSummary, ResolverFailure,
    ResolverFailureApiKeyEnvNotFound, ResolverFailureCustom, ServiceCause,
    ServiceCauseUnknownWebStreamSource, ServiceContext, ServiceContextUnknownWebStreamSource,
    ServiceFailure, ServiceOperation, StreamTimeoutKind, Utf8Failure, ValueExtFailure,
    ValueExtFailureCustom, ValueExtFailurePropertyNotFound, ValueExtFailurePropertyValueNotOfType,
    ValueExtFailureValueNotOfType, WebcFailure, WebcFailureResponseFailedInvalidJson,
    WebcFailureResponseFailedNotJson, WebcFailureResponseFailedStatus,
};
use std::{error::Error, str::Utf8Error, string::FromUtf8Error};

pub(super) fn genai_failure(error: &genai::Error, service: &str) -> ServiceFailure {
    let mut context = Vec::new();
    let cause = foreign_cause(error, &mut context);
    ServiceFailure {
        service: service.to_owned(),
        operation: ServiceOperation::Chat,
        context,
        cause: Box::new(cause),
    }
}

/// For refresh client construction, send, and body I/O only. Local response
/// decoding must use `JsonInputFailure`, never reqwest's serde decoder.
pub fn service_from_reqwest(
    error: &reqwest::Error,
    service: &str,
    operation: ServiceOperation,
) -> ServiceFailure {
    ServiceFailure {
        service: service.to_owned(),
        operation,
        context: Vec::new(),
        cause: Box::new(ServiceCause::Reqwest(reqwest_failure(error))),
    }
}

pub fn provider_stop(reason: &StopReason) -> ProviderStop {
    match reason {
        StopReason::Completed(raw) => ProviderStop::Completed(raw.clone()),
        StopReason::MaxTokens(raw) => ProviderStop::MaxTokens(raw.clone()),
        StopReason::ToolCall(raw) => ProviderStop::ToolCall(raw.clone()),
        StopReason::ContentFilter(raw) => ProviderStop::ContentFilter(raw.clone()),
        StopReason::StopSequence(raw) => ProviderStop::StopSequence(raw.clone()),
        StopReason::Other(raw) => ProviderStop::Unclassified(raw.clone()),
    }
}

fn model(model: &genai::ModelIden) -> ProviderModel {
    ProviderModel {
        adapter: adapter(model.adapter_kind),
        name: model.model_name.to_string(),
    }
}

fn message_role(role: &ChatRole) -> ProviderMessageRole {
    match role {
        ChatRole::System => ProviderMessageRole::System,
        ChatRole::User => ProviderMessageRole::User,
        ChatRole::Assistant => ProviderMessageRole::Assistant,
        ChatRole::Tool => ProviderMessageRole::Tool,
    }
}

fn adapter(adapter: AdapterKind) -> ProviderAdapter {
    match adapter {
        AdapterKind::OpenAI => ProviderAdapter::OpenAi,
        AdapterKind::OpenAIResp => ProviderAdapter::OpenAiResp,
        AdapterKind::Gemini => ProviderAdapter::Gemini,
        AdapterKind::GeminiIx => ProviderAdapter::GeminiIx,
        AdapterKind::Anthropic => ProviderAdapter::Anthropic,
        AdapterKind::Fireworks => ProviderAdapter::Fireworks,
        AdapterKind::Together => ProviderAdapter::Together,
        AdapterKind::Groq => ProviderAdapter::Groq,
        AdapterKind::Aihubmix => ProviderAdapter::Aihubmix,
        AdapterKind::Kimi => ProviderAdapter::Kimi,
        AdapterKind::Mimo => ProviderAdapter::Mimo,
        AdapterKind::Moonshot => ProviderAdapter::Moonshot,
        AdapterKind::Nebius => ProviderAdapter::Nebius,
        AdapterKind::Xai => ProviderAdapter::Xai,
        AdapterKind::DeepSeek => ProviderAdapter::DeepSeek,
        AdapterKind::Mistral => ProviderAdapter::Mistral,
        AdapterKind::Zai => ProviderAdapter::Zai,
        AdapterKind::BigModel => ProviderAdapter::BigModel,
        AdapterKind::Aliyun => ProviderAdapter::Aliyun,
        AdapterKind::QwenCloud => ProviderAdapter::QwenCloud,
        AdapterKind::Baidu => ProviderAdapter::Baidu,
        AdapterKind::Cohere => ProviderAdapter::Cohere,
        AdapterKind::Ollama => ProviderAdapter::Ollama,
        AdapterKind::OllamaCloud => ProviderAdapter::OllamaCloud,
        AdapterKind::Omlx => ProviderAdapter::Omlx,
        AdapterKind::Vertex => ProviderAdapter::Vertex,
        AdapterKind::GithubCopilot => ProviderAdapter::GithubCopilot,
        AdapterKind::OpenCodeGo => ProviderAdapter::OpenCodeGo,
        AdapterKind::BedrockApi => ProviderAdapter::BedrockApi,
        AdapterKind::OpenRouter => ProviderAdapter::OpenRouter,
        AdapterKind::AtlasCloud => ProviderAdapter::AtlasCloud,
        AdapterKind::Requesty => ProviderAdapter::Requesty,
        AdapterKind::ApiRoute => ProviderAdapter::ApiRoute,
        AdapterKind::Heabsy => ProviderAdapter::Heabsy,
        AdapterKind::Runcrate => ProviderAdapter::Runcrate,
        AdapterKind::CloudGpu => ProviderAdapter::CloudGpu,
        AdapterKind::Cheapaimodel => ProviderAdapter::Cheapaimodel,
        AdapterKind::MiniMax => ProviderAdapter::MiniMax,
        AdapterKind::Custom(id) => ProviderAdapter::Custom(id),
    }
}

fn foreign_cause(
    mut error: &(dyn Error + 'static),
    context: &mut Vec<ServiceContext>,
) -> ServiceCause {
    loop {
        if let Some(genai) = error.downcast_ref::<genai::Error>() {
            use genai::Error as E;
            let leaf = match genai {
                E::ChatReqHasNoMessages { model_iden } => {
                    GenaiFailure::ChatReqHasNoMessages(model(model_iden))
                }
                E::NoChatResponse { model_iden } => GenaiFailure::NoChatResponse(model(model_iden)),
                E::RequiresApiKey { model_iden } => GenaiFailure::RequiresApiKey(model(model_iden)),
                E::NoAuthResolver { model_iden } => GenaiFailure::NoAuthResolver(model(model_iden)),
                E::NoAuthData { model_iden } => GenaiFailure::NoAuthData(model(model_iden)),
                E::LastChatMessageIsNotUser {
                    model_iden,
                    actual_role,
                } => GenaiFailure::LastChatMessageIsNotUser(GenaiFailureLastChatMessageIsNotUser {
                    model: model(model_iden),
                    actual_role: message_role(actual_role),
                }),
                E::MessageRoleNotSupported { model_iden, role } => {
                    GenaiFailure::MessageRoleNotSupported(GenaiFailureMessageRoleNotSupported {
                        model: model(model_iden),
                        role: message_role(role),
                    })
                }
                E::MessageContentTypeNotSupported { model_iden, cause } => {
                    GenaiFailure::MessageContentTypeNotSupported(
                        GenaiFailureMessageContentTypeNotSupported {
                            model: model(model_iden),
                            detail: (*cause).to_owned(),
                        },
                    )
                }
                E::JsonModeWithoutInstruction => GenaiFailure::JsonModeWithoutInstruction,
                E::VerbosityParsing { actual } => {
                    GenaiFailure::VerbosityParsing(GenaiFailureVerbosityParsing {
                        actual: actual.clone(),
                    })
                }
                E::ReasoningParsingError { actual } => {
                    GenaiFailure::ReasoningParsing(GenaiFailureReasoningParsing {
                        actual: actual.clone(),
                    })
                }
                E::ServiceTierParsing { actual } => {
                    GenaiFailure::ServiceTierParsing(GenaiFailureServiceTierParsing {
                        actual: actual.clone(),
                    })
                }
                E::PromptCacheRetentionParsing { actual } => {
                    GenaiFailure::PromptCacheRetentionParsing(
                        GenaiFailurePromptCacheRetentionParsing {
                            actual: actual.clone(),
                        },
                    )
                }
                E::InvalidJsonResponseElement { info } => GenaiFailure::InvalidJsonResponseElement(
                    GenaiFailureInvalidJsonResponseElement {
                        detail: (*info).to_owned(),
                    },
                ),
                E::ModelMapperFailed { model_iden, cause } => {
                    GenaiFailure::ModelMapperFailed(GenaiFailureModelMapperFailed {
                        model: model(model_iden),
                        cause: resolver(cause),
                    })
                }
                E::WebAdapterCall {
                    adapter_kind,
                    webc_error,
                } => {
                    context.push(ServiceContext::GenaiWebAdapterCall(adapter(*adapter_kind)));
                    error = webc_error;
                    continue;
                }
                E::WebModelCall {
                    model_iden,
                    webc_error,
                } => {
                    context.push(ServiceContext::GenaiWebModelCall(model(model_iden)));
                    error = webc_error;
                    continue;
                }
                E::WebStream {
                    model_iden,
                    error: source,
                    cause: _,
                } => {
                    context.push(ServiceContext::GenaiWebStream(model(model_iden)));
                    error = source.as_ref();
                    continue;
                }
                E::ChatResponseGeneration {
                    model_iden,
                    response_body,
                    cause,
                    request_payload: _,
                } => GenaiFailure::ChatResponseGeneration(GenaiFailureChatResponseGeneration {
                    model: model(model_iden),
                    response_body: response_body.to_string(),
                    detail: cause.clone(),
                }),
                E::ChatResponse { model_iden, body } => {
                    GenaiFailure::ChatResponse(GenaiFailureChatResponse {
                        model: model(model_iden),
                        body: body.to_string(),
                    })
                }
                E::StreamParse {
                    model_iden,
                    serde_error,
                } => GenaiFailure::StreamParse(GenaiFailureStreamParse {
                    model: model(model_iden),
                    cause: foreign_json(serde_error),
                }),
                E::HttpError {
                    status,
                    canonical_reason,
                    body,
                    headers,
                } => GenaiFailure::HttpError(GenaiFailureHttpError {
                    status: u32::from(status.as_u16()),
                    canonical_reason: canonical_reason.clone(),
                    body: body.clone(),
                    headers: response_headers(headers),
                }),
                E::Resolver {
                    model_iden,
                    resolver_error,
                } => GenaiFailure::Resolver(GenaiFailureResolver {
                    model: model(model_iden),
                    cause: resolver(resolver_error),
                }),
                E::AdapterNotSupported {
                    adapter_kind,
                    feature,
                } => GenaiFailure::AdapterNotSupported(GenaiFailureAdapterNotSupported {
                    adapter: adapter(*adapter_kind),
                    feature: feature.clone(),
                }),
                E::CacheBreakpointNoEligibleContent { model_iden, scope } => {
                    GenaiFailure::CacheBreakpointNoEligibleContent(
                        GenaiFailureCacheBreakpointNoEligibleContent {
                            model: model(model_iden),
                            scope: (*scope).to_owned(),
                        },
                    )
                }
                E::AdapterKindMismatch {
                    bound,
                    requested,
                    model,
                } => GenaiFailure::AdapterKindMismatch(GenaiFailureAdapterKindMismatch {
                    bound: adapter(*bound),
                    requested: adapter(*requested),
                    model: model.clone(),
                }),
                E::Internal(detail) => GenaiFailure::Internal(GenaiFailureInternal {
                    detail: detail.clone(),
                }),
                E::ClientBuildFail { cause } => {
                    GenaiFailure::ClientBuildFail(GenaiFailureClientBuildFail {
                        detail: cause.clone(),
                    })
                }
                E::JsonValueExt(cause) => GenaiFailure::JsonValueExt(value_ext(cause)),
                E::SerdeJson(cause) => GenaiFailure::SerdeJson(foreign_json(cause)),
            };
            return ServiceCause::Genai(leaf);
        }
        if let Some(timeout) = error.downcast_ref::<genai::webc::StreamTimeout>() {
            return ServiceCause::StreamTimeout(match timeout {
                genai::webc::StreamTimeout::Headers => StreamTimeoutKind::Headers,
                genai::webc::StreamTimeout::Read => StreamTimeoutKind::Read,
            });
        }
        if let Some(error) = error.downcast_ref::<reqwest::Error>() {
            return ServiceCause::Reqwest(reqwest_failure(error));
        }
        if let Some(error) = error.downcast_ref::<Utf8Error>() {
            return ServiceCause::Utf8(utf8(*error));
        }
        if let Some(error) = error.downcast_ref::<FromUtf8Error>() {
            return ServiceCause::Utf8(utf8(error.utf8_error()));
        }
        if let Some(error) = error.downcast_ref::<genai::webc::Error>() {
            use genai::webc::Error as E;
            return match error {
                E::ResponseFailedNotJson { content_type, body } => ServiceCause::Webc(
                    WebcFailure::ResponseFailedNotJson(WebcFailureResponseFailedNotJson {
                        content_type: content_type.clone(),
                        body: body.clone(),
                    }),
                ),
                E::ResponseFailedInvalidJson { body, cause } => ServiceCause::Webc(
                    WebcFailure::ResponseFailedInvalidJson(WebcFailureResponseFailedInvalidJson {
                        body: body.clone(),
                        detail: cause.clone(),
                    }),
                ),
                E::ResponseFailedStatus {
                    status,
                    body,
                    headers,
                } => ServiceCause::Webc(WebcFailure::ResponseFailedStatus(
                    WebcFailureResponseFailedStatus {
                        status: u32::from(status.as_u16()),
                        body: body.clone(),
                        headers: response_headers(headers),
                    },
                )),
                E::JsonValueExt(cause) => {
                    ServiceCause::Webc(WebcFailure::JsonValueExt(value_ext(cause)))
                }
                E::Reqwest(error) => ServiceCause::Reqwest(reqwest_failure(error)),
            };
        }
        if let Some(error) = error.downcast_ref::<std::io::Error>() {
            return ServiceCause::Io(ral_core::io::failure(error));
        }
        if let Some(error) = error.downcast_ref::<url::ParseError>() {
            return ServiceCause::Url(url_failure(*error));
        }
        if let Some(error) = error.downcast_ref::<value_ext::JsonValueExtError>() {
            return ServiceCause::ValueExt(value_ext(error));
        }
        if let Some(error) = error.downcast_ref::<serde_json::Error>() {
            return ServiceCause::Json(foreign_json(error));
        }
        let detail = error.to_string();
        match error.source() {
            Some(source) => {
                context.push(ServiceContext::UnknownWebStreamSource(
                    ServiceContextUnknownWebStreamSource { detail },
                ));
                error = source;
            }
            None => {
                return ServiceCause::UnknownWebStreamSource(ServiceCauseUnknownWebStreamSource {
                    detail,
                });
            }
        }
    }
}

fn resolver(error: &genai::resolver::Error) -> ResolverFailure {
    match error {
        genai::resolver::Error::ApiKeyEnvNotFound { env_name } => {
            ResolverFailure::ApiKeyEnvNotFound(ResolverFailureApiKeyEnvNotFound {
                env_name: env_name.clone(),
            })
        }
        genai::resolver::Error::ResolverAuthDataNotSingleValue => {
            ResolverFailure::AuthDataNotSingleValue
        }
        genai::resolver::Error::Custom(detail) => ResolverFailure::Custom(ResolverFailureCustom {
            detail: detail.clone(),
        }),
    }
}

fn value_ext(error: &value_ext::JsonValueExtError) -> ValueExtFailure {
    use value_ext::JsonValueExtError as E;
    match error {
        E::Custom(detail) => ValueExtFailure::Custom(ValueExtFailureCustom {
            detail: detail.clone(),
        }),
        E::PropertyNotFound(name) => {
            ValueExtFailure::PropertyNotFound(ValueExtFailurePropertyNotFound {
                name: name.clone(),
            })
        }
        E::PropertyValueNotOfType { name, not_of_type } => {
            ValueExtFailure::PropertyValueNotOfType(ValueExtFailurePropertyValueNotOfType {
                name: name.clone(),
                required_library_type: (*not_of_type).to_owned(),
            })
        }
        E::ValueNotOfType(required) => {
            ValueExtFailure::ValueNotOfType(ValueExtFailureValueNotOfType {
                required_library_type: (*required).to_owned(),
            })
        }
        E::SerdeJson(error) => ValueExtFailure::SerdeJson(foreign_json(error)),
    }
}

fn foreign_json(error: &serde_json::Error) -> ForeignJsonFailure {
    ForeignJsonFailure {
        category: match error.classify() {
            serde_json::error::Category::Io => ForeignJsonCategory::Io,
            serde_json::error::Category::Syntax => ForeignJsonCategory::Syntax,
            serde_json::error::Category::Data => ForeignJsonCategory::Data,
            serde_json::error::Category::Eof => ForeignJsonCategory::Eof,
        },
        line: Count::from(error.line()),
        column: Count::from(error.column()),
        io_kind: error.io_error_kind().map(Into::into),
        detail: error.to_string(),
    }
}

fn response_headers(headers: &reqwest::header::HeaderMap) -> Vec<ProviderResponseHeader> {
    let mut names: Vec<_> = headers.keys().collect();
    names.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    names
        .into_iter()
        .flat_map(|name| {
            headers
                .get_all(name)
                .iter()
                .map(move |value| ProviderResponseHeader {
                    name: name.as_str().to_owned(),
                    value: value.as_bytes().into(),
                })
        })
        .collect()
}

fn utf8(error: Utf8Error) -> Utf8Failure {
    Utf8Failure {
        valid_up_to: Count::from(error.valid_up_to()),
        error_len: error.error_len().map(Count::from),
    }
}

fn url_failure(error: url::ParseError) -> ProviderUrlFailure {
    match error {
        url::ParseError::EmptyHost => ProviderUrlFailure::EmptyHost,
        url::ParseError::IdnaError => ProviderUrlFailure::Idna,
        url::ParseError::InvalidPort => ProviderUrlFailure::InvalidPort,
        url::ParseError::InvalidIpv4Address => ProviderUrlFailure::InvalidIpv4Address,
        url::ParseError::InvalidIpv6Address => ProviderUrlFailure::InvalidIpv6Address,
        url::ParseError::InvalidDomainCharacter => ProviderUrlFailure::InvalidDomainCharacter,
        url::ParseError::RelativeUrlWithoutBase => ProviderUrlFailure::RelativeWithoutBase,
        url::ParseError::RelativeUrlWithCannotBeABaseBase => {
            ProviderUrlFailure::RelativeWithCannotBeABaseBase
        }
        url::ParseError::SetHostOnCannotBeABaseUrl => ProviderUrlFailure::SetHostOnCannotBeABaseUrl,
        url::ParseError::Overflow => ProviderUrlFailure::Overflow,
        error => ProviderUrlFailure::Unclassified(ProviderUrlFailureUnclassified {
            detail: error.to_string(),
        }),
    }
}

fn reqwest_summary(error: &reqwest::Error) -> ReqwestSummary {
    let class = if error.is_builder() {
        ReqwestClass::Builder
    } else if error.is_redirect() {
        ReqwestClass::Redirect
    } else if let Some(status) = error.status() {
        ReqwestClass::Status(u32::from(status.as_u16()))
    } else if error.is_request() {
        ReqwestClass::Request
    } else if error.is_body() {
        ReqwestClass::Body
    } else if error.is_decode() {
        ReqwestClass::Decode
    } else if error.is_upgrade() {
        ReqwestClass::Upgrade
    } else {
        ReqwestClass::Unclassified(ReqwestClassUnclassified {
            detail: error.to_string(),
        })
    };
    ReqwestSummary {
        class,
        timeout: error.is_timeout(),
        connect: error.is_connect(),
        dns: error.is_dns(),
        url: error.url().map(|url| url.as_str().to_owned()),
    }
}

/// Reqwest client/send/body failures at this provider boundary. Locally decoded
/// OAuth payloads never enter this foreign source-chain projection.
pub fn reqwest_failure(error: &reqwest::Error) -> ReqwestFailure {
    ReqwestFailure {
        summary: reqwest_summary(error),
        sources: reqwest_sources(error.source()),
    }
}

fn reqwest_sources(mut source: Option<&(dyn Error + 'static)>) -> Vec<ReqwestSource> {
    let mut sources = Vec::new();
    while let Some(error) = source {
        let projected = if let Some(error) = error.downcast_ref::<reqwest::Error>() {
            ReqwestSource::Reqwest(reqwest_summary(error))
        } else if let Some(error) = error.downcast_ref::<std::io::Error>() {
            let cause = ral_core::io::failure(error);
            let consumed =
                !cause.context.is_empty() || !matches!(cause.leaf.as_ref(), IoLeaf::Os(_));
            sources.push(ReqwestSource::Io(cause));
            if consumed {
                break;
            }
            source = error.source();
            continue;
        } else if let Some(error) = error.downcast_ref::<serde_json::Error>() {
            ReqwestSource::Json(foreign_json(error))
        } else if let Some(error) = error.downcast_ref::<url::ParseError>() {
            ReqwestSource::Url(url_failure(*error))
        } else if let Some(error) = error.downcast_ref::<Utf8Error>() {
            ReqwestSource::Utf8(utf8(*error))
        } else if let Some(error) = error.downcast_ref::<FromUtf8Error>() {
            ReqwestSource::Utf8(utf8(error.utf8_error()))
        } else {
            ReqwestSource::Foreign(ReqwestSourceForeign {
                detail: error.to_string(),
            })
        };
        sources.push(projected);
        source = error.source();
    }
    sources
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
