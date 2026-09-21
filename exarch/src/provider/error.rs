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
        /// The HTTP status when this came from a 5xx response; `None` for a
        /// transport fault (connect/timeout/DNS) or a locally-raised idle
        /// timeout, neither of which ever reached one.  What `summary`/the
        /// TUI headline key their label on.
        status: Option<u16>,
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
    /// The turn was cut off short of the model finishing.  Raised by
    /// [`crate::agent::Avatar::deliberate`] *after* it appends the partial
    /// assistant message, so a re-prompt keeps that work as context.  Boxed:
    /// a stall's cause is a `ProviderError` in turn.
    Truncated { cause: Box<CutShort> },
    /// Anything else, rendered raw.
    Other(String),
}

/// Why an assistant turn ended before the model chose to stop.
///
/// The two have different remedies — a cap is the user's ceiling to raise, a
/// stall is nobody's to fix and is survived — so they are one type with two
/// arms rather than one reason string the reader has to interpret.
#[derive(Debug, Clone)]
pub enum CutShort {
    /// The output ceiling, under the provider's own raw stop reason.
    OutputCap { stop_reason: String },
    /// The stream broke after text or reasoning had already reached the
    /// caller.  The failure rides along whole — a stall is a provider error
    /// that arrived too late to retry, and the user is owed the same detail
    /// either way.
    Stalled(ProviderError),
}

impl ProviderError {
    /// A fault this boundary raised itself, having never reached a response:
    /// retryable, and with neither a status nor a provider body to show.
    pub(crate) fn local_transient(cause: impl Into<String>) -> Self {
        Self::Transient {
            cause: cause.into(),
            attempts: 1,
            body: None,
            status: None,
        }
    }

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
                status: Some(status.as_u16()),
            },
            Fault::Status { status, body, .. } => Self::Api {
                status: Some(status.as_u16()),
                model: model.to_string(),
                message: msg,
                body,
            },
            Fault::Transport(detail) => Self::Transient {
                cause: detail.unwrap_or(msg),
                attempts: 1,
                body: None,
                status: None,
            },
            Fault::Terminal(inner) => Self::Other(inner.unwrap_or(msg)),
        }
    }
}

/// The leaf of a genai error that decides how the boundary recovers.  The walk
/// is total — the `_ => Terminal` floor means a genai variant added upstream
/// surfaces raw instead of being silently misclassified.
enum Fault<'a> {
    /// A completed response with a non-2xx status, reached through any of the
    /// four shapes genai reports one by: `HttpError`; `ResponseFailedStatus`;
    /// an `HttpError` boxed in `WebStream`, the streaming path's version of
    /// the same shape; or a mid-stream `ChatResponse` frame whose status lives
    /// in its JSON body and so carries no headers to read a `retry-after`
    /// from.
    Status {
        status: StatusCode,
        headers: Option<&'a HeaderMap>,
        body: Option<Body>,
    },
    /// A `reqwest` fault that never reached a status — connect, timeout, a
    /// body that dropped or would not decode.  Retryable.
    ///
    /// The payload is [`root_cause`]: reqwest maps *every* mid-stream body
    /// failure through one `Display` string ("error decoding response body"),
    /// so without the leaf under it a reset peer, an h2 `GOAWAY` and this
    /// client's own read timeout are one indistinguishable message.
    Transport(Option<String>),
    /// Neither status nor transport: a request built wrong, an auth gap, a 2xx
    /// whose body was not the JSON genai required.  Retrying only re-loses.
    /// The payload is the unwrapped cause's own message, when one was found,
    /// so `Other` skips genai's wrapper text.
    Terminal(Option<String>),
}

impl<'a> Fault<'a> {
    fn of(err: &'a genai::Error) -> Self {
        match err {
            genai::Error::HttpError {
                status,
                body,
                headers,
                ..
            } => Self::status(*status, Some(headers), body),
            genai::Error::WebModelCall { webc_error, .. }
            | genai::Error::WebAdapterCall { webc_error, .. } => Self::of_webc(webc_error),
            genai::Error::WebStream { error, .. } => Self::of_boxed(error.as_ref()),
            genai::Error::ChatResponse { body, .. } => json_status_code(body)
                .and_then(|code| StatusCode::from_u16(code).ok())
                .map_or(Fault::Terminal(None), |status| Fault::Status {
                    status,
                    headers: None,
                    body: Some(Box::new(unwrap_relay(body.clone()))),
                }),
            _ => Fault::Terminal(None),
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
            _ => Fault::Terminal(None),
        }
    }

    /// A `WebStream` boxes its cause: a `genai::Error`, walked in turn, or a
    /// bare `reqwest::Error`.  Anything else is terminal, its own message kept.
    fn of_boxed(err: &'a (dyn std::error::Error + 'static)) -> Self {
        if let Some(genai) = err.downcast_ref::<genai::Error>() {
            return Fault::of(genai);
        }
        if let Some(reqwest) = err.downcast_ref::<reqwest::Error>() {
            return Self::of_reqwest(reqwest);
        }
        Fault::Terminal(Some(err.to_string()))
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
            Fault::Transport(root_cause(err))
        } else {
            Fault::Terminal(None)
        }
    }

    fn status(status: StatusCode, headers: Option<&'a HeaderMap>, body: &str) -> Self {
        Fault::Status {
            status,
            headers,
            body: serde_json::from_str(body)
                .ok()
                .map(|b| Box::new(unwrap_relay(b))),
        }
    }
}

/// The deepest `source` under an error: the one link that names what actually
/// went wrong, every wrapper above it being generic.  `None` when the error is
/// its own root.
fn root_cause(err: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut leaf = None;
    let mut source = err.source();
    while let Some(link) = source {
        leaf = Some(link.to_string());
        source = link.source();
    }
    leaf
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
/// whose `raw` is that provider's own body, verbatim, as a JSON string.  The
/// envelope names no fault the inner body does not, so the boundary keeps the
/// inner body and carries the upstream provider's name across as `provider`;
/// otherwise every reader — the classifier, `summary`, the TUI readout — gets
/// "Provider returned error" and a wall of escaped JSON.
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

/// The server's explicit back-off, read from the headers genai retains on
/// `ResponseFailedStatus` rather than scraped out of `Display`. RFC 9110
/// allows `Retry-After` as either delta-seconds or an HTTP-date; both forms
/// reach real providers, so both are read.
fn retry_after_header(headers: &HeaderMap) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(secs) = value.parse() {
        return Some(Duration::from_secs(secs));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    parse_http_date(value, now)
}

/// `Retry-After`'s HTTP-date form, the IMF-fixdate RFC 9110 requires a sender
/// use: `Sun, 06 Nov 1994 08:49:37 GMT`. A date already past floors at `0`
/// rather than going negative — the caller backs off immediately.
fn parse_http_date(s: &str, now_secs: u64) -> Option<Duration> {
    let (_, rest) = s.split_once(", ")?;
    let mut parts = rest.split_ascii_whitespace();
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':');
    let hour: i64 = hms.next()?.parse().ok()?;
    let min: i64 = hms.next()?.parse().ok()?;
    let sec: i64 = hms.next()?.parse().ok()?;
    if parts.next()? != "GMT" || hms.next().is_some() || parts.next().is_some() {
        return None;
    }
    let epoch_secs = days_from_civil(year, month, day)
        .checked_mul(86_400)?
        .checked_add(hour * 3600 + min * 60 + sec)?;
    #[allow(
        clippy::cast_possible_wrap,
        reason = "now_secs is real wall-clock time, far below i64::MAX"
    )]
    let wait = epoch_secs - now_secs as i64;
    Some(Duration::from_secs(wait.max(0).unsigned_abs()))
}

/// Days since the Unix epoch for a Gregorian civil date — Howard Hinnant's
/// `days_from_civil`, the standard branch-free algorithm; valid for every
/// date this era's HTTP servers will ever send.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
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
            Self::Transient { body, status, .. } => {
                with_body_message(transient_label(*status), body.as_deref())
            }
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
            Self::Truncated { cause } => format!("reply cut off: {}", cause.summary()),
            Self::Other(s) => first_line(s).to_string(),
        }
    }
}

impl CutShort {
    /// One line naming the cut and what it was: the remedy is the renderer's
    /// business, not this string's.
    pub fn summary(&self) -> String {
        match self {
            Self::OutputCap { stop_reason } => format!("output cap ({stop_reason})"),
            Self::Stalled(cause) => format!("stream stalled ({})", cause.summary()),
        }
    }
}

/// `Transient`'s kind label, shared with the TUI's structured renderer: a 5xx
/// response is a server failure, anything without a status never got one — a
/// dropped connection, a timeout, an idle stream.
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

/// The trailing lines a multi-line cause carries are genai's
/// `Cause:`/`Status:`/`Body:` framing, which the summary deliberately drops.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s).trim_end()
}

/// The flat [`Self::summary`] is the only rendering; the structured block is
/// the renderer's, not this impl's.
impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
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

    /// The non-streamed (`exec_chat`) failure shape genai returns, which
    /// carries its status only as a typed `StatusCode`.
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
    fn web_stream_http(status: StatusCode, headers: HeaderMap) -> genai::Error {
        genai::Error::WebStream {
            model_iden: iden("m"),
            cause: "stream open failed".into(),
            error: Box::new(genai::Error::HttpError {
                status,
                canonical_reason: status.canonical_reason().unwrap_or("").into(),
                body: String::new(),
                headers: Box::new(headers),
            }),
        }
    }

    /// A gateway's envelope is not the error: the upstream body inside
    /// `metadata.raw` is, and the relaying provider's name survives with it.
    #[test]
    fn from_genai_unwraps_gateway_relay_envelope() {
        let e = ProviderError::from_genai(
            &genai::Error::HttpError {
                status: StatusCode::BAD_REQUEST,
                canonical_reason: "Bad Request".into(),
                body: r#"{"error":{"code":"400","message":"Provider returned error","metadata":{"raw":"{\"error\":{\"message\":\"unsupported schema keyword\",\"param\":\"tools\",\"type\":\"invalid_request_error\"}}","provider_name":"ModelRun"}}}"#.into(),
                headers: Box::new(HeaderMap::new()),
            },
            "m",
        );
        assert_eq!(
            e.summary(),
            "api error 400: unsupported schema keyword",
            "the relayed message should replace the envelope's"
        );
        let ProviderError::Api { body: Some(b), .. } = &e else {
            panic!("expected Api with a body, got {e:?}")
        };
        let obj = error_object(b).expect("unwrapped body keeps an error object");
        assert_eq!(obj.get("param").and_then(serde_json::Value::as_str), Some("tools"));
        assert_eq!(
            obj.get("provider").and_then(serde_json::Value::as_str),
            Some("ModelRun"),
            "the relaying provider's name is the one thing the envelope owns"
        );
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

    /// genai's streaming `HttpError` carries its own response headers, not
    /// just `ResponseFailedStatus`'s — so a 429 hit mid-stream honors the
    /// server's `retry-after` exactly as a non-streamed one does.
    #[test]
    fn from_genai_classifies_429_rate_limit_via_web_stream_http() {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, HeaderValue::from_static("11"));
        let e = ProviderError::from_genai(
            &web_stream_http(StatusCode::TOO_MANY_REQUESTS, headers),
            "gpt-5.5",
        );
        match e {
            ProviderError::RateLimited { retry_after, .. } => {
                assert_eq!(retry_after, Some(Duration::from_secs(11)));
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
        let e = ProviderError::from_genai(
            &web_stream_http(StatusCode::BAD_REQUEST, HeaderMap::new()),
            "deepseek-v4-pro",
        );
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
    /// `StatusCode` or the backoff loop never retries a non-streamed 5xx.
    #[test]
    fn from_genai_classifies_a_non_streamed_5xx_as_transient() {
        let e = ProviderError::from_genai(
            &web_model_call(StatusCode::SERVICE_UNAVAILABLE, HeaderMap::new()),
            "anthropic/claude-opus-4",
        );
        assert!(
            matches!(e, ProviderError::Transient { .. }),
            "a non-streamed 503 must classify Transient, got {e:?}"
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

    #[test]
    fn body_detail_reads_nested_json_error() {
        let body = r#"{"error":{"message":"invalid api key"}}"#;
        assert_eq!(body_detail(body), "invalid api key");
    }

    #[test]
    fn body_detail_reads_flat_json_error() {
        let body = r#"{"message":"model not found"}"#;
        assert_eq!(body_detail(body), "model not found");
    }

    #[test]
    fn body_detail_caps_a_json_message_too() {
        let long = "x".repeat(BODY_DETAIL_CAP + 50);
        let body = serde_json::json!({ "error": { "message": long } }).to_string();
        let out = body_detail(&body);
        assert_eq!(out.chars().count(), BODY_DETAIL_CAP + 1);
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn body_detail_names_an_html_page_instead_of_spilling_it() {
        let body = "<!DOCTYPE html><html><body>502 Bad Gateway</body></html>";
        assert_eq!(
            body_detail(body),
            "the endpoint returned a web page, not an error message"
        );
    }

    #[test]
    fn body_detail_falls_back_to_first_line_of_plain_text() {
        let body =
            "\nupstream connect error or disconnect/reset before headers\nsome more detail\n";
        assert_eq!(
            body_detail(body),
            "upstream connect error or disconnect/reset before headers"
        );
    }

    #[test]
    fn body_detail_caps_a_long_first_line() {
        let line = "x".repeat(BODY_DETAIL_CAP + 50);
        let expected = format!("{}…", "x".repeat(BODY_DETAIL_CAP));
        assert_eq!(body_detail(&line), expected);
    }

    #[test]
    fn body_detail_names_an_empty_body() {
        assert_eq!(body_detail(""), "the endpoint sent no detail");
        assert_eq!(body_detail("   \n  "), "the endpoint sent no detail");
    }
}
