//! The provider-boundary error taxonomy and the genai fault classifier.
//!
//! [`ProviderError`] is the structured failure every provider call surfaces;
//! [`ProviderError::from_genai`] classifies a typed genai error into one by
//! walking its variants down to the leaf that decides recovery (see
//! [`Fault`]). The retry driver in the parent module keys its backoff on the
//! variant, and the TUI renders the [`crate::event::ProviderErrorRecord`]
//! mirror.

use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use std::fmt;
use std::time::Duration;

/// Structured failure at the provider boundary.
///
/// Each variant carries the fields the renderer needs to print a
/// labelled, wrapped error block, and the retry loop uses the variant
/// itself to decide whether to back off and try again.  The classifier
/// [`ProviderError::from_genai`] keys on genai's public typed variants
/// and their structured `StatusCode` / `HeaderMap`: a non-2xx status is
/// read directly off `HttpError`, `WebModelCall(ResponseFailedStatus)`,
/// or the `HttpError` boxed inside a streaming `WebStream`, and a
/// mid-stream JSON error frame (`ChatResponse`) is parsed from its
/// `serde_json::Value` body.  A transport fault with no status is the
/// `reqwest::Error` boxed inside `WebStream` or carried by `WebModelCall`;
/// anything with neither a status nor a transport leaf surfaces raw.  See
/// [`Fault`] for the structural walk.
#[derive(Debug, Clone)]
pub enum ProviderError {
    /// The user (or a chained signal handler) raised the cancel flag
    /// while the request was in flight.  The `&'static str` records
    /// *where* in the lifecycle the cancel landed so the UI can pin the
    /// blame correctly.
    Cancelled(&'static str),
    /// Network/stream/5xx error — the request can be retried with the
    /// same payload.  `attempts` is the total number of attempts made
    /// before giving up (always `>= 1`).
    Transient {
        cause: String,
        attempts: u32,
        /// The parsed JSON error body when the provider returned one (the
        /// HTTP 5xx path); `None` for transport / parse faults that never
        /// produced a JSON body.  The renderer uses it for a structured
        /// display.
        body: Option<serde_json::Value>,
    },
    /// Rate-limited (HTTP 429).  Honoured by the retry loop with a
    /// dedicated sleep tier; if `retry_after` is present the server
    /// asked for that wait explicitly.
    RateLimited {
        retry_after: Option<Duration>,
        cause: String,
        /// The parsed JSON error body when the provider returned one (the
        /// HTTP 429 path); `None` for a 429 surfaced without a JSON body.
        /// The renderer uses it for a structured display.
        body: Option<serde_json::Value>,
    },
    /// 4xx response from the provider (auth, bad request, model not
    /// found, etc.).  Not retried — the user has to change something.
    Api {
        status: Option<u16>,
        model: String,
        message: String,
        url: Option<String>,
        /// The parsed JSON error body when the provider returned one (the
        /// HTTP 4xx path); `None` when no JSON body was carried.  The
        /// renderer uses it for a structured display.
        body: Option<serde_json::Value>,
    },
    /// The assistant turn was cut off before the model finished — the
    /// output cap ([`crate::provider::CutShort::OutputCap`]) or a
    /// mid-stream stall ([`crate::provider::CutShort::Stalled`]).  Raised
    /// by [`crate::agent::Agent::apply`] **after** appending the partial
    /// assistant message, so re-prompting with `continue` preserves the
    /// partial work as transcript context.
    Truncated {
        /// The cut-off cause as a string: the provider's normalised stop
        /// reason (`"max_tokens"`) for an output-cap truncation, or the
        /// stream cause for a stall.
        reason: String,
    },
    /// Anything else: rendered raw so the user has the original error
    /// text to work with.
    Other(String),
}

impl ProviderError {
    /// Classify a typed genai error into a provider-boundary failure.
    ///
    /// The verdict is purely structural: [`Fault::of`] walks the error's
    /// typed variants down to the leaf that decides recovery — an HTTP
    /// status, a transport-level `reqwest` fault, or neither — and this
    /// turns that leaf into a public failure.  A recovered [`StatusCode`]
    /// drives the 429 / 5xx / 4xx split, honouring a `retry-after` header
    /// when the variant carries one; a transport leaf is the canonical
    /// retryable `Transient`; anything with no leaf to stand on surfaces
    /// raw as `Other`.  No classification rests on the `Display` string.
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
            Fault::Status { status, body, .. } if status.is_server_error() => {
                Self::Transient {
                    cause: msg,
                    attempts: 1,
                    body,
                }
            }
            // Any remaining non-success status (4xx, redirects) is a
            // request the user must change — not retryable.
            Fault::Status { status, body, .. } => Self::Api {
                status: Some(status.as_u16()),
                model: model.to_string(),
                message: msg.clone(),
                url: extract_url(&msg),
                body,
            },
            // A genuine transport fault — connect, timeout, or a body that
            // dropped or failed to decode mid-flight — with no HTTP status
            // ever received.  The canonical retryable failure.
            Fault::Transport => Self::Transient {
                cause: msg,
                attempts: 1,
                body: None,
            },
            // No status, no transport leaf: an input / auth / parse fault
            // the user must act on.  Rendered raw.
            Fault::Terminal => Self::Other(msg),
        }
    }
}

/// The structural anatomy of a genai fault: the leaf that decides how the
/// provider boundary recovers, recovered by walking the typed variants
/// rather than scraping a `Display` string.  Every genai error reaches
/// exactly one arm — the `_ => Terminal` floor makes the walk total, so a
/// new genai variant defaults to "surface it" instead of being silently
/// misclassified.
enum Fault<'a> {
    /// A completed HTTP response carrying a non-2xx status, reached by one
    /// of the four shapes genai reports it through:
    ///
    /// * `HttpError` — the raw HTTP-level error.
    /// * `WebModelCall`/`WebAdapterCall(ResponseFailedStatus)` — the
    ///   non-streamed `exec_chat` path; `headers` carry `retry-after`.
    /// * `WebStream` boxing an `HttpError` — the streaming path's initial
    ///   non-2xx response, recovered by recursion through [`Fault::of`].
    /// * `ChatResponse` — a mid-stream JSON error frame whose status lives
    ///   in `body["error"]["code"]` / `body["code"]`.
    ///
    /// `body` is the provider's JSON error frame parsed best-effort; a
    /// non-JSON body (an HTML 5xx page) leaves it `None` and the caller
    /// falls back to the textual `cause`.
    Status {
        status: StatusCode,
        headers: Option<&'a HeaderMap>,
        body: Option<serde_json::Value>,
    },
    /// A transport-level `reqwest` fault that never reached a status —
    /// connect, timeout, a malformed request, or a body that dropped or
    /// failed to decode mid-flight.  The streaming path boxes it inside
    /// `WebStream`; the non-streamed path carries it as `WebModelCall`/
    /// `WebAdapterCall(Reqwest)`.  Retryable.
    Transport,
    /// No status and no transport leaf: a request built wrong, an auth
    /// gap, a provider contract breach (a 2xx whose body was not the JSON
    /// genai required), or a parse / decode corruption.  Retrying only
    /// re-loses, so it surfaces raw.
    Terminal,
}

impl<'a> Fault<'a> {
    /// Walk a genai error to the leaf that determines its recovery.
    fn of(err: &'a genai::Error) -> Self {
        match err {
            genai::Error::HttpError { status, body, .. } => Self::status(*status, None, body),
            genai::Error::WebModelCall { webc_error, .. }
            | genai::Error::WebAdapterCall { webc_error, .. } => Self::of_webc(webc_error),
            // The streaming path boxes its leaf as a `BoxError`: an initial
            // non-2xx response as a `genai::Error::HttpError`, an in-flight
            // fault as a `reqwest::Error`, a corrupt body as a UTF-8 error.
            genai::Error::WebStream { error, .. } => Self::of_boxed(error.as_ref()),
            // A mid-stream JSON error frame whose status lives in the body.
            genai::Error::ChatResponse { body, .. } => json_status_code(body)
                .and_then(|code| StatusCode::from_u16(code).ok())
                .map_or(Fault::Terminal, |status| Fault::Status {
                    status,
                    headers: None,
                    body: Some(body.clone()),
                }),
            // Input, auth, mapping, and stream-parse errors carry nothing
            // to recover or retry.
            _ => Fault::Terminal,
        }
    }

    /// The leaf inside a `WebModelCall` / `WebAdapterCall` webc error.
    fn of_webc(err: &'a genai::webc::Error) -> Self {
        match err {
            genai::webc::Error::ResponseFailedStatus {
                status,
                headers,
                body,
            } => Self::status(*status, Some(headers), body),
            genai::webc::Error::Reqwest(e) => Self::of_reqwest(e),
            // A 2xx whose body was missing or not the JSON genai required
            // is a provider contract breach, not a transport fault.
            _ => Fault::Terminal,
        }
    }

    /// The leaf boxed inside a `WebStream` cause: a `genai::Error` (walked
    /// in turn) or a bare `reqwest::Error`; anything else — a UTF-8
    /// corruption — is terminal.
    fn of_boxed(err: &'a (dyn std::error::Error + 'static)) -> Self {
        if let Some(genai) = err.downcast_ref::<genai::Error>() {
            return Fault::of(genai);
        }
        if let Some(reqwest) = err.downcast_ref::<reqwest::Error>() {
            return Self::of_reqwest(reqwest);
        }
        Fault::Terminal
    }

    /// A `reqwest::Error` is transport-retryable only for the fault classes
    /// a re-issue can plausibly clear; a builder / redirect fault is the
    /// caller's to fix and stays terminal.
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

    /// A status leaf with its headers and a best-effort JSON body.
    fn status(status: StatusCode, headers: Option<&'a HeaderMap>, body: &str) -> Self {
        Fault::Status {
            status,
            headers,
            body: parse_json_body(body),
        }
    }
}

/// Parse a provider error body string as JSON, best-effort.  A non-JSON
/// body (e.g. an HTML 5xx page) yields `None` so the caller falls back to
/// the textual `cause`.
fn parse_json_body(s: &str) -> Option<serde_json::Value> {
    serde_json::from_str(s).ok()
}

/// The error-detail object inside a provider JSON body.  Providers wrap
/// differently — `OpenAI` nests the detail under `error`, Anthropic sends
/// `{"type":"error","error":{…}}` — so prefer the inner `error` object,
/// falling back to the body itself when there is no such nesting.  The one
/// home of the nest-or-flat convention: classification
/// ([`json_status_code`]), the cross-boundary [`ProviderError::summary`],
/// and the structured renderer all read their fields through it.
pub(crate) fn error_object(
    body: &serde_json::Value,
) -> Option<&serde_json::Map<String, serde_json::Value>> {
    body.get("error")
        .and_then(serde_json::Value::as_object)
        .or_else(|| body.as_object())
}

/// Read a status code from a provider JSON error body, accepting both the
/// nested `{"error":{"code":NNN}}` and the flat `{"code":NNN}` shapes.
fn json_status_code(body: &serde_json::Value) -> Option<u16> {
    error_object(body)?
        .get("code")
        .and_then(serde_json::Value::as_u64)
        .and_then(|c| u16::try_from(c).ok())
}

/// Parse the structured `Retry-After` header as a whole-second delay.
/// genai retains the response headers on `ResponseFailedStatus`, so the
/// server's explicit back-off request is read directly rather than
/// scraped from the `Display` string.
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
    /// The bare cause line, without the variant's framing — what a
    /// [`crate::provider::CutShort::Stalled`] note wants to quote.  For the
    /// transient/other failures a committed stall produces this is the raw
    /// stream cause (e.g. the `reqwest` read-timeout text genai surfaces as
    /// a `"Web stream error"`); for anything else it falls back to the full
    /// `Display`.
    pub(crate) fn brief(&self) -> String {
        match self {
            Self::Transient { cause, .. } | Self::RateLimited { cause, .. } => {
                cause.clone()
            }
            Self::Other(s) => s.clone(),
            other => other.to_string(),
        }
    }

    /// A single-line human summary, for a failure that crosses an agent
    /// boundary as a flat string — a sub-agent's outcome a parent receives.
    /// The full structured [`crate::tui::line::provider_error`] block is
    /// unreachable there, and the verbose [`fmt::Display`] (which splices the
    /// raw HTTP `Body:` JSON into `cause`) would land as an unreadable wall in
    /// the one-line breadcrumb.  So this prefers the provider's own parsed
    /// `body` message — the same field the structured block surfaces — over
    /// `cause`, falling back to the variant's kind label when no body message
    /// is present.
    pub fn summary(&self) -> String {
        match self {
            Self::Cancelled(where_) => format!("cancelled {where_}"),
            Self::Transient { body, .. } => {
                with_body_message("web stream failed", body.as_ref())
            }
            Self::RateLimited { body, .. } => {
                with_body_message("rate limited", body.as_ref())
            }
            Self::Api {
                status,
                message,
                body,
                ..
            } => {
                let detail = body
                    .as_ref()
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

/// The kind label, suffixed with the provider's own JSON message when the
/// `body` carries one (`rate limited: Weekly usage limit reached…`).
fn with_body_message(kind: &str, body: Option<&serde_json::Value>) -> String {
    match body.and_then(body_message) {
        Some(m) => format!("{kind}: {m}"),
        None => kind.to_string(),
    }
}

/// The human message a provider's JSON error body carries, if any — the
/// error object's `message`, read through the shared [`error_object`].
fn body_message(body: &serde_json::Value) -> Option<String> {
    error_object(body)?
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// The first line of a possibly multi-line cause — a one-line breadcrumb
/// cannot carry the rest, and the trailing lines are usually the genai
/// `Cause:`/`Status:`/`Body:` framing the summary deliberately drops.
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

/// Best-effort parse of `retry-after: <secs>` (or similar) embedded in
/// the error string — the fallback for `RateLimited` errors whose typed
/// variant carries no structured `Retry-After` header.  genai surfaces
/// this inconsistently; missing is fine, the loop falls back to
/// exponential backoff.
fn parse_retry_after(msg: &str) -> Option<Duration> {
    let needle = "retry-after";
    // Slice the lowercased copy, never index the original with an offset
    // taken from it: a non-ASCII char whose lowercasing changes its byte
    // length (e.g. `İ`) shifts every later offset, so `&msg[i..]` could
    // land mid-character and panic.  The seconds are ASCII digits,
    // identical in both copies.
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

/// Pull the first `https?://…` substring from `msg` so the renderer
/// can label the endpoint that failed.  Trims a trailing `)` or `,` so
/// genai's `for url (https://…)` shape gives back the bare URL.
fn extract_url(msg: &str) -> Option<String> {
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

    /// A `ModelIden` for the typed-error fixtures below.  The classifier
    /// never reads it, so the adapter kind is arbitrary.
    fn iden(model: &str) -> ModelIden {
        ModelIden::new(AdapterKind::Anthropic, model)
    }

    /// The non-streamed (`exec_chat`) failure shape: a non-2xx status
    /// surfaces as `WebModelCall(ResponseFailedStatus { status, headers })`
    /// — the path the compaction summary takes.  This is what
    /// [`crate::provider::Engine::summarize`] hands to `from_genai`.
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

    /// The streaming (`exec_chat_stream`) failure shape: the initial
    /// non-2xx response becomes `HttpError`, boxed as the `BoxError` cause
    /// of `WebStream`.  This is what `resp.stream.next()` hands to
    /// `from_genai`.
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

    /// A `WebStream` whose boxed cause is neither a recoverable status nor
    /// a transport `reqwest::Error` (here a bare string) has no leaf to
    /// stand on.  It surfaces raw as `Other` rather than being retried on a
    /// `Display`-string guess — the classification is structural, not
    /// textual.
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

    /// A provider contract breach — a 2xx response whose body was not the
    /// JSON genai required — carries no status and no transport leaf, so it
    /// is terminal `Other`.  Its `Display` mentions "body", the word a
    /// substring heuristic would have wrongly seized on to retry it; the
    /// structural walk reads the typed variant instead and does not.
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

    /// A 429 on the non-streamed (compaction) path: `WebModelCall`
    /// carrying a `ResponseFailedStatus` with a `Retry-After` header.  The
    /// typed match keys on `StatusCode::TOO_MANY_REQUESTS` and reads the
    /// wait straight off the structured header.
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

    /// A rate-limit failure that crosses an agent boundary (a sub-agent's
    /// outcome a parent receives) must `summary()` to one clean line carrying
    /// the provider's own JSON message — never the verbose `Display`, which
    /// splices the raw HTTP `Body:` JSON and would render as a wall of JSON in
    /// the parent's one-line breadcrumb.
    #[test]
    fn summary_reads_body_message_not_the_json_wall() {
        let e = ProviderError::RateLimited {
            retry_after: None,
            cause: "Web stream error for model 'glm-5.2 (adapter: OpenAI)'.\n\
                    Cause: HTTP error.\nStatus: 429 Too Many Requests\nBody:\n  \
                    {\"error\":{\"message\":\"Weekly usage limit reached. Resets in 4 days.\"}}"
                .into(),
            body: Some(serde_json::json!({
                "type": "error",
                "error": {
                    "type": "GoUsageLimitError",
                    "message": "Weekly usage limit reached. Resets in 4 days.",
                },
            })),
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

    /// With no parsed body the summary degrades to the bare kind label,
    /// never the verbose `cause` — the breadcrumb has no room for it.
    #[test]
    fn summary_without_body_is_the_kind_label() {
        let e = ProviderError::RateLimited {
            retry_after: None,
            cause: "Web stream error.\nStatus: 429".into(),
            body: None,
        };
        assert_eq!(e.summary(), "rate limited");
    }

    /// The `DeepSeek` failure from the session log: a hard 400 arriving on
    /// the streaming path (`WebStream` boxing `HttpError`) must classify
    /// as a non-retryable `Api` error, not `Transient` — otherwise the
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

    /// A variant with no structured status and no transport leaf — an
    /// input error here — must surface raw as `Other`.
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

    /// A mid-stream SSE error frame (`ChatResponse`) whose body is the
    /// `OpenRouter` `{"error":{"code":NNN}}` JSON shape — the status is read
    /// structurally from the `serde_json::Value` and routed like any other
    /// 4xx.
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

    /// The flat `{"code":NNN}` JSON shape resolves the same way as the
    /// nested form, and a JSON 429 routes to `RateLimited`, not `Api`.
    #[test]
    fn json_status_code_reads_both_shapes() {
        assert_eq!(
            json_status_code(&serde_json::json!({"error": {"code": 503}})),
            Some(503)
        );
        assert_eq!(
            json_status_code(&serde_json::json!({"code": 429})),
            Some(429)
        );
        assert_eq!(
            json_status_code(&serde_json::json!({"message": "no code"})),
            None
        );
    }

    /// A non-streamed `exec_chat` 5xx surfaces as
    /// `WebModelCall(ResponseFailedStatus { status })`, whose `Display`
    /// renders the status as `"… status code '503'"` with no
    /// machine-parseable `status:` token; classification must therefore read
    /// the typed `StatusCode`.  This pins that such a 503 is `Transient` so
    /// the backoff loop retries it.
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

    /// A non-ASCII char whose lowercasing changes its byte length
    /// (`İ` → `i̇`, one byte longer) before the matched `retry-after`
    /// needle must not panic: the slice is taken from the lowercased copy,
    /// not the shorter original.
    #[test]
    fn parse_retry_after_survives_length_changing_lowercase() {
        assert_eq!(
            parse_retry_after("İ retry-after: 9 seconds"),
            Some(Duration::from_secs(9))
        );
    }
}
