//! Provider-boundary failures, and the classifier that maps genai's typed
//! errors onto them.
//!
//! Classification is structural: [`Fault`] walks an error's variants down to the
//! leaf that decides recovery, never scraping a `Display` string. The retry
//! driver in `retry.rs` keys its backoff on the resulting variant, and
//! [`crate::agent::event::ProviderErrorRecord`] mirrors it for the TUI.

use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use std::fmt;
use std::time::Duration;

/// A provider's JSON error frame, boxed: `genai` turns on `serde_json`'s
/// `preserve_order`, so a `Value` is 72 bytes here, and three variants carry
/// one — the width every provider call's success path pays for.
type Body = Box<serde_json::Value>;

/// Structured failure at the provider boundary.  The variant alone tells the
/// retry loop whether to back off, so misclassifying one is expensive.
#[derive(Debug, Clone)]
pub enum ProviderError {
    /// Cancelled in flight; the string names the lifecycle site, so the UI
    /// can pin the blame.
    Cancelled(&'static str),
    /// Retryable with the same payload: network, stream, or 5xx.  `attempts`
    /// is the total made before giving up, stamped by `retry.rs`.
    Transient {
        cause: String,
        attempts: u32,
        /// The provider's JSON error frame when it sent one; `None` leaves
        /// the renderer only `cause`.
        body: Option<Body>,
    },
    /// HTTP 429.  `retry_after` is the server's own explicit wait, when it
    /// asked for one; `retry.rs` gives this variant a longer leash than a
    /// generic transient.
    RateLimited {
        retry_after: Option<Duration>,
        cause: String,
        body: Option<Body>,
    },
    /// A non-success status that is neither 429 nor 5xx: auth, bad request,
    /// model not found.  Never retried — the user has to change something.
    ///
    /// The failing endpoint is [`extract_url`] of `message`, not a field — a
    /// stored copy could disagree with the message beside it.
    Api {
        status: Option<u16>,
        model: String,
        message: String,
        body: Option<Body>,
    },
    /// The turn was cut off short of the model finishing — output cap or
    /// mid-stream stall ([`crate::provider::CutShort`]).  Raised by
    /// [`crate::agent::Agent::deliberate`] *after* it appends the partial
    /// assistant message, so a re-prompt keeps that work as context.
    Truncated {
        /// The provider's normalised stop reason (`"max_tokens"`) for a cap,
        /// the stream cause for a stall.
        reason: String,
    },
    /// Anything else, rendered raw.
    Other(String),
}

impl ProviderError {
    /// Classify a typed genai error into a provider-boundary failure.
    ///
    /// The whole verdict rests on the leaf [`Fault::of`] recovers, never on
    /// the `Display` string.
    pub fn from_genai(err: &genai::Error, model: &str) -> Self {
        let msg = err.to_string();
        match Fault::of(err) {
            Fault::Status {
                status,
                headers,
                body,
            } if status == StatusCode::TOO_MANY_REQUESTS => {
                let retry_after = headers
                    .and_then(retry_after_header)
                    .or_else(|| parse_retry_after(&msg));
                Self::RateLimited {
                    retry_after,
                    cause: msg,
                    body,
                }
            }
            Fault::Status { status, body, .. } if status.is_server_error() => Self::Transient {
                cause: msg,
                attempts: 1,
                body,
            },
            Fault::Status { status, body, .. } => Self::Api {
                status: Some(status.as_u16()),
                model: model.to_string(),
                message: msg,
                body,
            },
            Fault::Transport => Self::Transient {
                cause: msg,
                attempts: 1,
                body: None,
            },
            Fault::Terminal => Self::Other(msg),
        }
    }
}

/// The leaf of a genai error that decides how the boundary recovers.  The walk
/// is total — the `_ => Terminal` floor means a genai variant added upstream
/// surfaces raw instead of being silently misclassified.
enum Fault<'a> {
    /// A completed response with a non-2xx status, reached through any of the
    /// four shapes genai reports one by: `HttpError`; `ResponseFailedStatus`,
    /// the only shape carrying `headers` and so the only `retry-after` source;
    /// an `HttpError` boxed in `WebStream`; or a mid-stream `ChatResponse`
    /// frame whose status lives in its JSON body.
    Status {
        status: StatusCode,
        headers: Option<&'a HeaderMap>,
        body: Option<Body>,
    },
    /// A `reqwest` fault that never reached a status — connect, timeout, a
    /// body that dropped or would not decode.  Retryable.
    Transport,
    /// Neither status nor transport: a request built wrong, an auth gap, a 2xx
    /// whose body was not the JSON genai required.  Retrying only re-loses.
    Terminal,
}

impl<'a> Fault<'a> {
    fn of(err: &'a genai::Error) -> Self {
        match err {
            genai::Error::HttpError { status, body, .. } => Self::status(*status, None, body),
            genai::Error::WebModelCall { webc_error, .. }
            | genai::Error::WebAdapterCall { webc_error, .. } => Self::of_webc(webc_error),
            genai::Error::WebStream { error, .. } => Self::of_boxed(error.as_ref()),
            genai::Error::ChatResponse { body, .. } => json_status_code(body)
                .and_then(|code| StatusCode::from_u16(code).ok())
                .map_or(Fault::Terminal, |status| Fault::Status {
                    status,
                    headers: None,
                    body: Some(Box::new(body.clone())),
                }),
            _ => Fault::Terminal,
        }
    }

    fn of_webc(err: &'a genai::webc::Error) -> Self {
        match err {
            genai::webc::Error::ResponseFailedStatus {
                status,
                headers,
                body,
            } => Self::status(*status, Some(headers), body),
            genai::webc::Error::Reqwest(e) => Self::of_reqwest(e),
            _ => Fault::Terminal,
        }
    }

    /// A `WebStream` boxes its cause: a `genai::Error`, walked in turn, or a
    /// bare `reqwest::Error`.  Anything else — a UTF-8 corruption — is terminal.
    fn of_boxed(err: &'a (dyn std::error::Error + 'static)) -> Self {
        if let Some(genai) = err.downcast_ref::<genai::Error>() {
            return Fault::of(genai);
        }
        if let Some(reqwest) = err.downcast_ref::<reqwest::Error>() {
            return Self::of_reqwest(reqwest);
        }
        Fault::Terminal
    }

    /// Only the fault classes a re-issue can plausibly clear count as
    /// transport; a builder or redirect fault is the caller's to fix.
    fn of_reqwest(err: &reqwest::Error) -> Self {
        if err.is_connect()
            || err.is_timeout()
            || err.is_request()
            || err.is_body()
            || err.is_decode()
        {
            Fault::Transport
        } else {
            Fault::Terminal
        }
    }

    fn status(status: StatusCode, headers: Option<&'a HeaderMap>, body: &str) -> Self {
        Fault::Status {
            status,
            headers,
            body: serde_json::from_str(body).ok(),
        }
    }
}

/// The error-detail object inside a provider JSON body.  Providers wrap
/// differently — `OpenAI` nests the detail under `error`, Anthropic sends
/// `{"type":"error","error":{…}}` — so this is the single home of the
/// nest-or-flat convention, read by classification, by
/// [`ProviderError::summary`], and by the TUI's structured renderer alike.
pub(crate) fn error_object(
    body: &serde_json::Value,
) -> Option<&serde_json::Map<String, serde_json::Value>> {
    body.get("error")
        .and_then(serde_json::Value::as_object)
        .or_else(|| body.as_object())
}

/// The status code in a provider JSON error body, nested or flat.
fn json_status_code(body: &serde_json::Value) -> Option<u16> {
    error_object(body)?
        .get("code")
        .and_then(serde_json::Value::as_u64)
        .and_then(|c| u16::try_from(c).ok())
}

/// The server's explicit back-off in whole seconds, read from the headers genai
/// retains on `ResponseFailedStatus` rather than scraped out of `Display`.
fn retry_after_header(headers: &HeaderMap) -> Option<Duration> {
    let secs: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs))
}

impl ProviderError {
    /// One line, for a failure crossing an agent boundary as a flat string —
    /// the `AgentOutcome::Failed` a parent receives from a sub-agent.  The TUI's
    /// structured block is unreachable there and [`fmt::Display`] splices the raw
    /// HTTP `Body:` JSON into `cause`, so this prefers the provider's own parsed
    /// message and falls back to the kind label.
    pub fn summary(&self) -> String {
        match self {
            Self::Cancelled(where_) => format!("cancelled {where_}"),
            Self::Transient { body, .. } => with_body_message("web stream failed", body.as_deref()),
            Self::RateLimited { body, .. } => with_body_message("rate limited", body.as_deref()),
            Self::Api {
                status,
                message,
                body,
                ..
            } => {
                let detail = body
                    .as_deref()
                    .and_then(body_message)
                    .unwrap_or_else(|| first_line(message).to_string());
                match status {
                    Some(s) => format!("api error {s}: {detail}"),
                    None => format!("api error: {detail}"),
                }
            }
            Self::Truncated { reason } => format!("reply cut off ({reason})"),
            Self::Other(s) => first_line(s).to_string(),
        }
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

/// The trailing lines a multi-line cause carries are genai's
/// `Cause:`/`Status:`/`Body:` framing, which the summary deliberately drops.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s).trim_end()
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled(where_) => write!(f, "cancelled {where_}"),
            Self::Transient {
                cause, attempts, ..
            } => {
                if *attempts > 1 {
                    write!(f, "transient error after {attempts} attempts: {cause}")
                } else {
                    write!(f, "transient error: {cause}")
                }
            }
            Self::RateLimited { cause, .. } => {
                write!(f, "rate limited: {cause}")
            }
            Self::Api {
                status,
                model,
                message,
                ..
            } => match status {
                Some(s) => write!(f, "api error {s} ({model}): {message}"),
                None => write!(f, "api error ({model}): {message}"),
            },
            Self::Truncated { reason } => {
                write!(f, "reply cut off before completion (reason={reason})")
            }
            Self::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for ProviderError {}

/// The fallback for a 429 whose typed variant carries no `Retry-After` header;
/// genai surfaces it inconsistently, and missing is fine — the retry loop just
/// backs off exponentially instead.
fn parse_retry_after(msg: &str) -> Option<Duration> {
    let needle = "retry-after";
    // Slice the lowercased copy, never the original with an offset taken from
    // it: a char whose lowercasing changes byte length (`İ`) shifts every later
    // offset, so `&msg[i..]` could land mid-character and panic.
    let lower = msg.to_lowercase();
    let i = lower.find(needle)?;
    let tail = &lower[i + needle.len()..];
    let digits: String = tail
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    let secs: u64 = digits.parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// The first `https?://…` in `msg`, so the renderer can label the endpoint that
/// failed.  A trailing `)` or `,` is trimmed, so genai's `for url (https://…)`
/// shape gives back the bare URL.
pub(crate) fn extract_url(msg: &str) -> Option<String> {
    let i = msg.find("http://").or_else(|| msg.find("https://"))?;
    let tail = &msg[i..];
    let end = tail
        .find(|c: char| c.is_whitespace() || c == ')' || c == ',')
        .unwrap_or(tail.len());
    Some(tail[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::ModelIden;
    use genai::adapter::AdapterKind;
    use reqwest::header::HeaderValue;

    /// The classifier never reads the adapter kind, so it is arbitrary.
    fn iden(model: &str) -> ModelIden {
        ModelIden::new(AdapterKind::Anthropic, model)
    }

    /// The non-streamed (`exec_chat`) failure shape — what `Engine::summarize`,
    /// the compaction path, hands to `from_genai`.
    fn web_model_call(status: StatusCode, headers: HeaderMap) -> genai::Error {
        genai::Error::WebModelCall {
            model_iden: iden("m"),
            webc_error: genai::webc::Error::ResponseFailedStatus {
                status,
                body: String::new(),
                headers: Box::new(headers),
            },
        }
    }

    /// The streaming failure shape: the initial non-2xx becomes an `HttpError`
    /// boxed as `WebStream`'s cause, as `resp.stream.next()` yields it.
    fn web_stream_http(status: StatusCode) -> genai::Error {
        genai::Error::WebStream {
            model_iden: iden("m"),
            cause: "stream open failed".into(),
            error: Box::new(genai::Error::HttpError {
                status,
                canonical_reason: status.canonical_reason().unwrap_or("").into(),
                body: String::new(),
            }),
        }
    }

    /// A boxed cause with no typed leaf must not be retried on a
    /// `Display`-string guess.
    #[test]
    fn from_genai_classifies_web_stream_unknown_cause_as_other() {
        let e = ProviderError::from_genai(
            &genai::Error::WebStream {
                model_iden: iden("gpt-5.4"),
                cause: "error sending request for url".into(),
                error: Box::<dyn std::error::Error + Send + Sync>::from(
                    "error sending request for url",
                ),
            },
            "gpt-5.4",
        );
        assert!(
            matches!(e, ProviderError::Other(_)),
            "a WebStream with no typed leaf is terminal, got {e:?}"
        );
    }

    /// Its `Display` mentions "body", the word a substring heuristic would have
    /// seized on to retry it; the structural walk reads the variant and does not.
    #[test]
    fn from_genai_classifies_non_json_response_as_other() {
        let e = ProviderError::from_genai(
            &genai::Error::WebModelCall {
                model_iden: iden("m"),
                webc_error: genai::webc::Error::ResponseFailedNotJson {
                    content_type: "text/html".into(),
                    body: "<html>200 but not JSON</html>".into(),
                },
            },
            "m",
        );
        assert!(
            matches!(e, ProviderError::Other(_)),
            "a non-JSON 2xx body is terminal, got {e:?}"
        );
    }

    /// The wait comes straight off the structured header, not the message text.
    #[test]
    fn from_genai_classifies_429_rate_limit() {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, HeaderValue::from_static("7"));
        let e = ProviderError::from_genai(
            &web_model_call(StatusCode::TOO_MANY_REQUESTS, headers),
            "gpt-5.5",
        );
        match e {
            ProviderError::RateLimited { retry_after, .. } => {
                assert_eq!(retry_after, Some(Duration::from_secs(7)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    /// The verbose `Display` would land a wall of JSON in a parent agent's
    /// one-line breadcrumb.
    #[test]
    fn summary_reads_body_message_not_the_json_wall() {
        let e = ProviderError::RateLimited {
            retry_after: None,
            cause: "Web stream error for model 'glm-5.2 (adapter: OpenAI)'.\n\
                    Cause: HTTP error.\nStatus: 429 Too Many Requests\nBody:\n  \
                    {\"error\":{\"message\":\"Weekly usage limit reached. Resets in 4 days.\"}}"
                .into(),
            body: Some(Box::new(serde_json::json!({
                "type": "error",
                "error": {
                    "type": "GoUsageLimitError",
                    "message": "Weekly usage limit reached. Resets in 4 days.",
                },
            }))),
        };
        let s = e.summary();
        assert_eq!(
            s,
            "rate limited: Weekly usage limit reached. Resets in 4 days."
        );
        assert!(
            !s.contains('{'),
            "summary must not leak the raw JSON body: {s}"
        );
        assert!(!s.contains('\n'), "summary must stay one line: {s}");
    }

    /// Never the verbose `cause` — the breadcrumb has no room for it.
    #[test]
    fn summary_without_body_is_the_kind_label() {
        let e = ProviderError::RateLimited {
            retry_after: None,
            cause: "Web stream error.\nStatus: 429".into(),
            body: None,
        };
        assert_eq!(e.summary(), "rate limited");
    }

    /// A hard 400 on the streaming path must not classify `Transient`, or the
    /// retry loop burns its whole budget on an unfixable request.
    #[test]
    fn from_genai_classifies_400_wrapped_in_web_stream_error() {
        let e =
            ProviderError::from_genai(&web_stream_http(StatusCode::BAD_REQUEST), "deepseek-v4-pro");
        match e {
            ProviderError::Api { status, model, .. } => {
                assert_eq!(status, Some(400));
                assert_eq!(model, "deepseek-v4-pro");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn from_genai_classifies_unknown_as_other() {
        let e = ProviderError::from_genai(
            &genai::Error::ChatReqHasNoMessages {
                model_iden: iden("gpt-5.5"),
            },
            "gpt-5.5",
        );
        match e {
            ProviderError::Other(s) => assert!(s.contains("no messages")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    /// A mid-stream SSE frame in `OpenRouter`'s shape: the status is read out
    /// of the JSON body and routed like any other 4xx.
    #[test]
    fn from_genai_classifies_json_body_4xx_as_api() {
        let e = ProviderError::from_genai(
            &genai::Error::ChatResponse {
                model_iden: iden("anthropic/claude-opus-4"),
                body: serde_json::json!({"error": {"code": 400, "message": "bad request"}}),
            },
            "anthropic/claude-opus-4",
        );
        match e {
            ProviderError::Api { status, model, .. } => {
                assert_eq!(status, Some(400));
                assert_eq!(model, "anthropic/claude-opus-4");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    /// A status read out of the body routes exactly as a header one does:
    /// nested and flat bodies are both read, 429 reaches `RateLimited` rather
    /// than the generic `Api` path, and a body with no code stays terminal.
    #[test]
    fn from_genai_classifies_json_body_status_in_both_shapes() {
        let route = |body| {
            ProviderError::from_genai(
                &genai::Error::ChatResponse {
                    model_iden: iden("m"),
                    body,
                },
                "m",
            )
        };
        let nested = route(serde_json::json!({"error": {"code": 429}}));
        assert!(
            matches!(nested, ProviderError::RateLimited { .. }),
            "a JSON 429 must route to RateLimited, got {nested:?}"
        );
        let flat = route(serde_json::json!({"code": 503}));
        assert!(
            matches!(flat, ProviderError::Transient { .. }),
            "a flat JSON 503 must route to Transient, got {flat:?}"
        );
        let codeless = route(serde_json::json!({"message": "no code"}));
        assert!(
            matches!(codeless, ProviderError::Other(_)),
            "a JSON body with no code is terminal, got {codeless:?}"
        );
    }

    /// `ResponseFailedStatus` renders its status as `"… status code '503'"`,
    /// with no machine-parseable token — classification must read the typed
    /// `StatusCode` or the backoff loop never retries a compaction 5xx.
    #[test]
    fn from_genai_classifies_compaction_5xx_as_transient() {
        let e = ProviderError::from_genai(
            &web_model_call(StatusCode::SERVICE_UNAVAILABLE, HeaderMap::new()),
            "anthropic/claude-opus-4",
        );
        assert!(
            matches!(e, ProviderError::Transient { .. }),
            "compaction 503 must classify Transient, got {e:?}"
        );
    }

    /// `İ` → `i̇` is one byte longer, so a slice taken from the shorter original
    /// would land mid-character and panic.
    #[test]
    fn parse_retry_after_survives_length_changing_lowercase() {
        assert_eq!(
            parse_retry_after("İ retry-after: 9 seconds"),
            Some(Duration::from_secs(9))
        );
    }
}
