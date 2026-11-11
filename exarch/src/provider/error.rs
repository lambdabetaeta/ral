//! Retry-facing carriers; portable projections retain causes before rendering.

mod projection;
#[cfg(test)]
mod tests;

use super::reset;
use super::retry::Recovery;
use crate::clock;
use jiff::Timestamp;
pub use projection::{provider_stop, reqwest_failure, service_from_reqwest};
use ral_core::fact::{
    Count, CredentialFailure, ProviderCancelSite, ProviderContext, ProviderFailure,
    ProviderInstant, ProviderLeaf, ProviderLeafApi, ProviderLeafOutputCap, ProviderLeafOverflow,
    ProviderLeafTransient, ProviderLimit, ProviderRefusal, ProviderRefusalCause,
    ProviderRefusalCauseHeld, RecordingFailure, RetryableProviderCause,
    RetryableProviderCauseStreamEndedWithoutEnd, ServiceFailure,
};
use regex::Regex;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::{fmt, sync::LazyLock};

type Body = Box<serde_json::Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CancelSite {
    BeforeRequest,
    MidStream,
    Backoff,
}

impl fmt::Display for CancelSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BeforeRequest => "before request",
            Self::MidStream => "mid-stream",
            Self::Backoff => "during retry backoff",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderError {
    Unavailable(#[serde(with = "ral_core::data::datum::as_datum")] RecordingFailure),
    Cancelled(CancelSite),
    Transient {
        #[serde(with = "ral_core::data::datum::as_datum")]
        cause: RetryableProviderCause,
        attempts: u32,
    },
    Refused(Refusal),
    Api {
        model: String,
        #[serde(with = "ral_core::data::datum::as_datum")]
        cause: ServiceFailure,
    },
    Overflow {
        model: String,
        provider: Option<String>,
        sent: Option<u64>,
        limit: Option<u64>,
        listed: Option<u64>,
        #[serde(with = "ral_core::data::datum::as_datum")]
        cause: Box<ServiceFailure>,
    },
    Truncated {
        cause: Box<CutShort>,
    },
    Terminal(#[serde(with = "ral_core::data::datum::as_datum")] ServiceFailure),
    Credential(#[serde(with = "ral_core::data::datum::as_datum")] CredentialFailure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Limit {
    Rate,
    Allowance,
}

impl Limit {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rate => "rate limited",
            Self::Allowance => "usage limit reached",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Refusal {
    pub limit: Limit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<Timestamp>,
    pub received: Timestamp,
    #[serde(with = "ral_core::data::datum::as_datum")]
    pub cause: ProviderRefusalCause,
}

impl Refusal {
    pub fn headline(&self) -> String {
        match self.recovery() {
            Recovery::Deferred(at) => format!("{} until {}", self.limit.label(), clock::local(at)),
            Recovery::InPlace(_) => self.limit.label().to_string(),
        }
    }

    /// An unsent request gated by an earlier observed reset.
    pub fn held(
        limit: Limit,
        resets_at: Option<Timestamp>,
        received: Timestamp,
        service: &str,
        model: &str,
    ) -> Self {
        Self {
            limit,
            resets_at,
            received,
            cause: ProviderRefusalCause::Held(ProviderRefusalCauseHeld {
                service: service.to_owned(),
                model: model.to_owned(),
            }),
        }
    }

    pub fn failure(&self) -> ProviderRefusal {
        let instant = |at: Timestamp| ProviderInstant {
            seconds: at.as_second(),
            subsec_nanoseconds: at.subsec_nanosecond(),
        };
        ProviderRefusal {
            limit: match self.limit {
                Limit::Rate => ProviderLimit::Rate,
                Limit::Allowance => ProviderLimit::Allowance,
            },
            resets_at: self.resets_at.map(instant),
            received: instant(self.received),
            cause: self.cause.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(limit: Limit, resets_in: Option<std::time::Duration>) -> Self {
        let received = Timestamp::now();
        Self::held(
            limit,
            resets_in.map(|wait| received + wait),
            received,
            "fixture",
            "m",
        )
    }
}

/// Live recursion stays outside the portable Typed graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CutShort {
    OutputCap { stop_reason: String },
    Stalled(ProviderError),
}

impl ProviderError {
    pub fn stall_cause(&self) -> Option<&Self> {
        match self {
            Self::Truncated { cause } => match cause.as_ref() {
                CutShort::Stalled(error) => Some(error),
                CutShort::OutputCap { .. } => None,
            },
            _ => None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Cancelled(_) => "cancelled",
            Self::Transient { .. } => transient_label(self.status()),
            Self::Refused(r) => r.limit.label(),
            Self::Api { .. } => "api error",
            Self::Overflow { .. } => "context too long",
            Self::Truncated { .. } => "truncated",
            Self::Terminal(_) | Self::Credential(_) => "provider error",
            Self::Unavailable(_) => "recording unavailable",
        }
    }

    pub fn stream_ended_without_end(service: &str, model: &str) -> Self {
        Self::Transient {
            cause: RetryableProviderCause::StreamEndedWithoutEnd(
                RetryableProviderCauseStreamEndedWithoutEnd {
                    service: service.to_owned(),
                    model: model.to_owned(),
                },
            ),
            attempts: 1,
        }
    }

    /// `service` is the selected account's configured name, never inferred from an error.
    pub fn from_genai(error: &genai::Error, service: &str, model: &str) -> Self {
        let cause = projection::genai_failure(error, service);
        match Fault::of(error) {
            Fault::Response {
                status,
                headers,
                body,
            } => {
                let body = body.as_deref();
                if status == Some(StatusCode::TOO_MANY_REQUESTS) {
                    if body.is_some_and(reset::unwaitable) {
                        return Self::Api {
                            model: model.to_owned(),
                            cause,
                        };
                    }
                    let received = Timestamp::now();
                    return Self::Refused(Refusal {
                        limit: body.map_or(Limit::Rate, reset::limit),
                        resets_at: reset::at(
                            headers,
                            body,
                            cause.body().unwrap_or_default(),
                            received,
                        ),
                        received,
                        cause: ProviderRefusalCause::Service(cause),
                    });
                }
                if status.is_some_and(|status| status.is_server_error()) {
                    return Self::Transient {
                        cause: RetryableProviderCause::Service(cause),
                        attempts: 1,
                    };
                }
                let text = body
                    .and_then(body_message)
                    .or_else(|| cause.body().map(str::to_owned))
                    .unwrap_or_default();
                match overflow(body, &text) {
                    Some((sent, limit)) => Self::Overflow {
                        model: model.to_owned(),
                        provider: body
                            .and_then(error_object)
                            .and_then(|o| o.get("provider"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                        sent,
                        limit,
                        listed: None,
                        cause: Box::new(cause),
                    },
                    None => Self::Api {
                        model: model.to_owned(),
                        cause,
                    },
                }
            }
            Fault::Transport => Self::Transient {
                cause: RetryableProviderCause::Service(cause),
                attempts: 1,
            },
            Fault::Terminal => Self::Terminal(cause),
        }
    }

    /// Flatten only the live truncation links, retaining their order and count.
    pub fn failure(&self) -> ProviderFailure {
        let mut context = Vec::new();
        let mut current = self;
        let leaf = loop {
            break match current {
                Self::Truncated { cause } => match cause.as_ref() {
                    CutShort::Stalled(inner) => {
                        context.push(ProviderContext::Truncated);
                        current = inner;
                        continue;
                    }
                    CutShort::OutputCap { stop_reason } => {
                        ProviderLeaf::OutputCap(ProviderLeafOutputCap {
                            stop_reason: stop_reason.clone(),
                        })
                    }
                },
                Self::Unavailable(cause) => ProviderLeaf::Unavailable(cause.clone()),
                Self::Cancelled(site) => ProviderLeaf::Cancelled(match site {
                    CancelSite::BeforeRequest => ProviderCancelSite::BeforeRequest,
                    CancelSite::MidStream => ProviderCancelSite::MidStream,
                    CancelSite::Backoff => ProviderCancelSite::Backoff,
                }),
                Self::Transient { cause, attempts } => {
                    ProviderLeaf::Transient(ProviderLeafTransient {
                        cause: cause.clone(),
                        attempts: Count::from_u64(u64::from(*attempts)),
                    })
                }
                Self::Refused(cause) => ProviderLeaf::Refused(cause.failure()),
                Self::Api { model, cause } => ProviderLeaf::Api(ProviderLeafApi {
                    model: model.clone(),
                    cause: cause.clone(),
                }),
                Self::Overflow {
                    model,
                    provider,
                    sent,
                    limit,
                    listed,
                    cause,
                } => ProviderLeaf::Overflow(ProviderLeafOverflow {
                    model: model.clone(),
                    provider: provider.clone(),
                    sent: sent.map(Count::from_u64),
                    limit: limit.map(Count::from_u64),
                    listed: listed.map(Count::from_u64),
                    cause: cause.as_ref().clone(),
                }),
                Self::Terminal(cause) => ProviderLeaf::Terminal(cause.clone()),
                Self::Credential(cause) => ProviderLeaf::Credential(cause.clone()),
            };
        };
        ProviderFailure { context, leaf }
    }

    pub fn service_failure(&self) -> Option<&ServiceFailure> {
        match self {
            Self::Transient {
                cause: RetryableProviderCause::Service(cause),
                ..
            }
            | Self::Api { cause, .. }
            | Self::Terminal(cause)
            | Self::Refused(Refusal {
                cause: ProviderRefusalCause::Service(cause),
                ..
            }) => Some(cause),
            Self::Overflow { cause, .. } => Some(cause.as_ref()),
            _ => None,
        }
    }

    /// Response status only; a reqwest transport error is not a received response.
    pub fn status(&self) -> Option<u16> {
        let service = self.service_failure()?;
        if matches!(
            service.cause.as_ref(),
            ral_core::fact::ServiceCause::Reqwest(_)
        ) {
            return None;
        }
        match service.cause.as_ref() {
            ral_core::fact::ServiceCause::Genai(ral_core::fact::GenaiFailure::ChatResponse(
                response,
            )) => serde_json::from_str(&response.body)
                .ok()
                .as_ref()
                .and_then(json_status_code)
                .and_then(|code| StatusCode::from_u16(code).ok())
                .map(|status| status.as_u16()),
            _ => service
                .status()
                .and_then(|status| u16::try_from(status).ok()),
        }
    }

    /// Presentation/classification view; the original response remains in the service cause.
    pub fn body(&self) -> Option<Box<serde_json::Value>> {
        serde_json::from_str(self.service_failure()?.body()?)
            .ok()
            .map(unwrap_relay)
            .map(Box::new)
    }

    pub fn endpoint(&self) -> Option<&str> {
        self.service_failure()?.endpoint()
    }

    pub fn summary(&self) -> String {
        match self {
            Self::Cancelled(site) => format!("cancelled {site}"),
            Self::Transient { cause, .. } if self.status().is_none() => {
                format!("connection failed: {}", first_line(&cause.describe()))
            }
            Self::Transient { .. } => {
                with_body_message(transient_label(self.status()), self.body().as_deref())
            }
            Self::Refused(r) => with_body_message(&r.headline(), self.body().as_deref()),
            Self::Api { cause, .. } => {
                let detail = self
                    .body()
                    .as_deref()
                    .and_then(body_message)
                    .unwrap_or_else(|| first_line(&cause.describe()).to_owned());
                match self.status() {
                    Some(status) => format!("api error {status}: {detail}"),
                    None => format!("api error: {detail}"),
                }
            }
            Self::Overflow {
                provider,
                sent,
                limit,
                ..
            } => {
                let host = provider
                    .as_deref()
                    .map_or(String::new(), |p| format!(" for {p}"));
                let counts = match (sent, limit) {
                    (Some(s), Some(l)) => format!(": {s} tokens sent, {l} allowed"),
                    (Some(s), None) => format!(": {s} tokens sent"),
                    (None, Some(l)) => format!(": {l} tokens allowed"),
                    (None, None) => String::new(),
                };
                format!("context too long{host}{counts}")
            }
            Self::Truncated { cause } => format!("reply cut off: {}", cause.summary()),
            Self::Terminal(cause) => first_line(&cause.describe()).to_owned(),
            Self::Credential(cause) => first_line(&cause.describe()).to_owned(),
            Self::Unavailable(cause) => first_line(&cause.describe()).to_owned(),
        }
    }
}

impl CutShort {
    pub fn summary(&self) -> String {
        match self {
            Self::OutputCap { stop_reason } => format!("output cap ({stop_reason})"),
            Self::Stalled(cause) => format!("stream stalled ({})", cause.summary()),
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}
impl std::error::Error for ProviderError {}

/// Recovery policy reads live typed sources, separately from evidence capture.
enum Fault<'a> {
    Response {
        status: Option<StatusCode>,
        headers: Option<&'a reqwest::header::HeaderMap>,
        body: Option<Body>,
    },
    Transport,
    Terminal,
}

impl<'a> Fault<'a> {
    fn of(error: &'a genai::Error) -> Self {
        match error {
            genai::Error::HttpError {
                status,
                headers,
                body,
                ..
            } => Self::http(*status, Some(headers), body),
            genai::Error::WebModelCall { webc_error, .. }
            | genai::Error::WebAdapterCall { webc_error, .. } => Self::of_webc(webc_error),
            genai::Error::WebStream { error, .. } => Self::of_boxed(error.as_ref()),
            genai::Error::ChatResponse { body, .. } => Self::Response {
                status: json_status_code(body).and_then(|code| StatusCode::from_u16(code).ok()),
                headers: None,
                body: Some(Box::new(unwrap_relay(body.clone()))),
            },
            genai::Error::ChatReqHasNoMessages { .. }
            | genai::Error::LastChatMessageIsNotUser { .. }
            | genai::Error::MessageRoleNotSupported { .. }
            | genai::Error::MessageContentTypeNotSupported { .. }
            | genai::Error::JsonModeWithoutInstruction
            | genai::Error::VerbosityParsing { .. }
            | genai::Error::ReasoningParsingError { .. }
            | genai::Error::ServiceTierParsing { .. }
            | genai::Error::PromptCacheRetentionParsing { .. }
            | genai::Error::NoChatResponse { .. }
            | genai::Error::InvalidJsonResponseElement { .. }
            | genai::Error::RequiresApiKey { .. }
            | genai::Error::NoAuthResolver { .. }
            | genai::Error::NoAuthData { .. }
            | genai::Error::ModelMapperFailed { .. }
            | genai::Error::ChatResponseGeneration { .. }
            | genai::Error::StreamParse { .. }
            | genai::Error::Resolver { .. }
            | genai::Error::AdapterNotSupported { .. }
            | genai::Error::CacheBreakpointNoEligibleContent { .. }
            | genai::Error::AdapterKindMismatch { .. }
            | genai::Error::Internal(_)
            | genai::Error::ClientBuildFail { .. }
            | genai::Error::JsonValueExt(_)
            | genai::Error::SerdeJson(_) => Self::Terminal,
        }
    }

    fn of_webc(error: &'a genai::webc::Error) -> Self {
        match error {
            genai::webc::Error::ResponseFailedStatus {
                status,
                headers,
                body,
            } => Self::http(*status, Some(headers), body),
            genai::webc::Error::Reqwest(error) => Self::of_reqwest(error),
            genai::webc::Error::ResponseFailedNotJson { .. }
            | genai::webc::Error::ResponseFailedInvalidJson { .. }
            | genai::webc::Error::JsonValueExt(_) => Self::Terminal,
        }
    }

    fn of_boxed(error: &'a (dyn std::error::Error + 'static)) -> Self {
        if let Some(error) = error.downcast_ref::<genai::Error>() {
            return Self::of(error);
        }
        if error.is::<genai::webc::StreamTimeout>() {
            return Self::Transport;
        }
        if let Some(error) = error.downcast_ref::<reqwest::Error>() {
            return Self::of_reqwest(error);
        }
        Self::Terminal
    }

    fn of_reqwest(error: &reqwest::Error) -> Self {
        if error.is_connect()
            || error.is_timeout()
            || error.is_request()
            || error.is_body()
            || error.is_decode()
        {
            Self::Transport
        } else {
            Self::Terminal
        }
    }

    fn http(
        status: StatusCode,
        headers: Option<&'a reqwest::header::HeaderMap>,
        body: &str,
    ) -> Self {
        Self::Response {
            status: Some(status),
            headers,
            body: serde_json::from_str(body)
                .ok()
                .map(unwrap_relay)
                .map(Box::new),
        }
    }
}
/// Each provider's phrasing of a context overflow, naming `sent` and `limit`.
static OVERFLOW_PHRASINGS: LazyLock<[Regex; 4]> = LazyLock::new(|| {
    [
        r"input \((?<sent>\d+) tokens\) is longer than the model's context length \((?<limit>\d+) tokens\)",
        r"maximum context length is (?<limit>\d+) tokens.*?(?:resulted in|requested(?: about)?) (?<sent>\d+) tokens",
        r"prompt is too long: (?<sent>\d+) tokens > (?<limit>\d+) maximum",
        r"input token count \((?<sent>\d+)\) exceeds the maximum number of tokens allowed \((?<limit>\d+)\)",
    ]
    .map(|p| Regex::new(&format!("(?is){p}")).expect("a phrasing is a regex"))
});

/// `(sent, limit)` when an error body is a context overflow: a known phrasing
/// matched, or the body's own `type`/`code` says so.
fn overflow(body: Option<&serde_json::Value>, msg: &str) -> Option<(Option<u64>, Option<u64>)> {
    let text = body
        .and_then(body_message)
        .unwrap_or_else(|| msg.to_string());
    let count = |c: &regex::Captures, name| c.name(name).and_then(|m| m.as_str().parse().ok());
    OVERFLOW_PHRASINGS
        .iter()
        .find_map(|re| re.captures(&text))
        .map(|c| (count(&c, "sent"), count(&c, "limit")))
        .or_else(|| {
            let o = error_object(body?)?;
            ["type", "code"]
                .iter()
                .any(|k| {
                    o.get(*k).and_then(serde_json::Value::as_str) == Some("context_length_exceeded")
                })
                .then_some((None, None))
        })
}

/// The error-detail object inside a provider JSON body.  Providers wrap
/// differently — `OpenAI` nests the detail under `error`, Anthropic sends
/// `{"type":"error","error":{…}}`: so this is the single home of the
/// nest-or-flat convention, read by classification, by
/// [`ProviderError::summary`], and by the TUI's structured renderer alike.
pub(crate) fn error_object(
    body: &serde_json::Value,
) -> Option<&serde_json::Map<String, serde_json::Value>> {
    body.get("error")
        .and_then(serde_json::Value::as_object)
        .or_else(|| body.as_object())
}

/// The error-detail object of a body being rewritten, found by the same
/// nest-or-flat convention [`error_object`] reads.
fn error_object_mut(
    body: &mut serde_json::Value,
) -> Option<&mut serde_json::Map<String, serde_json::Value>> {
    if body.get("error").is_some_and(serde_json::Value::is_object) {
        return body
            .get_mut("error")
            .and_then(serde_json::Value::as_object_mut);
    }
    body.as_object_mut()
}

/// A gateway's relay envelope, replaced by the upstream body it carries.
/// `OpenRouter` answers an upstream provider's failure with a frame of its own
/// — `{"error":{"message":"Provider returned error","metadata":{"raw":"…"}}}` —
/// whose `raw` is that provider's own body, verbatim, as a JSON string. This
/// view carries the upstream provider's name as `provider`; the service cause
/// retains the original envelope.
fn unwrap_relay(body: serde_json::Value) -> serde_json::Value {
    let Some(metadata) = error_object(&body).and_then(|o| o.get("metadata")) else {
        return body;
    };
    let Some(inner) = metadata
        .get("raw")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .filter(serde_json::Value::is_object)
    else {
        return body;
    };
    let provider = metadata.get("provider_name").cloned();
    let mut inner = unwrap_relay(inner);
    if let (Some(name), Some(obj)) = (provider, error_object_mut(&mut inner)) {
        obj.insert("provider".to_string(), name);
    }
    inner
}

/// The status code in a provider JSON error body, nested or flat.
fn json_status_code(body: &serde_json::Value) -> Option<u16> {
    error_object(body)?
        .get("code")
        .and_then(serde_json::Value::as_u64)
        .and_then(|c| u16::try_from(c).ok())
}

/// `Transient`'s kind label, shared with the TUI's structured renderer: a 5xx
/// response is a server failure, anything without a status never got one — a
/// dropped connection, a timeout, a silent stream.
pub(crate) fn transient_label(status: Option<u16>) -> &'static str {
    if status.is_some() {
        "server error"
    } else {
        "connection failed"
    }
}

/// The kind label, suffixed with the provider's own JSON message when the body
/// carries one: `rate limited: Weekly usage limit reached…`.
fn with_body_message(kind: &str, body: Option<&serde_json::Value>) -> String {
    match body.and_then(body_message) {
        Some(m) => format!("{kind}: {m}"),
        None => kind.to_string(),
    }
}

fn body_message(body: &serde_json::Value) -> Option<String> {
    error_object(body)?
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Long enough to identify the problem, short enough not to wall-of-text a
/// sign-in or model-list error.
const BODY_DETAIL_CAP: usize = 200;

/// The readable detail inside a raw response body: the provider's own JSON
/// error message when there is one, else the body's first line.  A body that
/// is neither — an HTML error page — says so rather than spilling.
///
/// Every shape is capped on the way out, the parsed message included: an error
/// page from a proxy or WAF can reflect request context, and this string
/// reaches both the screen and the log, so no backend's prose is trusted past
/// a snippet.
pub(crate) fn body_detail(body: &str) -> String {
    let trimmed = body.trim();
    let detail = match serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .as_ref()
        .and_then(body_message)
    {
        Some(message) => message,
        None if trimmed.starts_with('<') => {
            "the endpoint returned a web page, not an error message".to_string()
        }
        None => match trimmed.lines().find(|l| !l.trim().is_empty()) {
            Some(line) => line.trim().to_string(),
            None => "the endpoint sent no detail".to_string(),
        },
    };
    if detail.chars().count() > BODY_DETAIL_CAP {
        format!(
            "{}…",
            detail.chars().take(BODY_DETAIL_CAP).collect::<String>()
        )
    } else {
        detail
    }
}

/// Presentation stays on one line; stored evidence remains whole.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s).trim_end()
}
