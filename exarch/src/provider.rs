//! LLM transport — thin wrapper over genai.
//!
//! `Provider::complete` takes already-rendered [`ChatMessage`]s and
//! streams one assistant reply.  The transcript owns history; the
//! provider only sends bytes and parses responses.

use crate::cancel;
use crate::credential::Credential;
use crate::oauth;
use clap::ValueEnum;
use futures_util::StreamExt;
use genai::Headers;
use genai::adapter::AdapterKind;
use genai::chat::{
    CacheControl, ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, MessageOptions,
    StreamEnd, Tool,
};
pub use genai::chat::{StopReason, ToolCall};
use genai::resolver::{AuthData, AuthResolver, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// Per-model max-tokens defaults live inside genai's own adapter table
// (e.g. `AnthropicAdapter::resolve_max_tokens` picks 32k for Opus 4.x,
// 64k for Sonnet/Haiku/4-5).  We previously pinned a hard 4k constant
// here which silently truncated long Opus turns; the fix is to *not
// override* genai's per-model resolution unless the user explicitly
// passes `--max-tokens`.  Provider::max_tokens_override carries the
// `Some` only when the user opted in.

/// Best-effort capability lookup for `model`.  Consults the OR
/// catalog regardless of provider — OR republishes upstream cards for
/// every vendor it routes, so a native Anthropic / OpenAI / DeepSeek
/// launch hits the same context-window / tokenizer / canonical-slug
/// data as the OR-fronted equivalent.  Missing entries default the
/// record so the banner renders `—` for unknown fields.
pub fn caps_for(model: &str) -> crate::pricing::ModelCaps {
    crate::pricing::caps(model).unwrap_or_default()
}

/// Retry budget for transient stream/network failures.  Three attempts
/// means up to two retries before surfacing the error.
const MAX_ATTEMPTS: u32 = 3;
/// Retry budget for rate-limit (429) failures.  Larger than the
/// transient budget: a 429 is the server explicitly asking us to wait,
/// not a broken request, and exarch's headless mode has no human to
/// re-prompt — so the provider loop is the only place rate-limit
/// resilience can live.
const RATE_LIMIT_MAX_ATTEMPTS: u32 = 6;
/// First backoff delay; subsequent delays double up to the per-error
/// ceiling.
const BASE_DELAY_MS: u64 = 750;
/// Backoff ceiling for transient failures — keeps the worst-case retry
/// window bounded (~12s over three attempts).
const MAX_DELAY_MS: u64 = 8_000;
/// Backoff ceiling for rate-limit failures.  Higher than the transient
/// ceiling so a server-supplied `retry-after` of up to half a minute is
/// honoured rather than clamped down to 8s.
const RATE_LIMIT_MAX_DELAY_MS: u64 = 30_000;
/// Idle timeout for the streaming request: the maximum gap we tolerate
/// *between* stream events (and, on the initial select, the bound on
/// connect + time-to-first-event).  It is reset on every received chunk
/// — not a total cap on the response — because [`tls::client`]
/// deliberately sets no request timeout to keep long, legitimately slow
/// completions alive.  Without it a connection that goes silent (TCP
/// open, bytes stopped) blocks `resp.stream.next()` forever; under the
/// terminal-bench harness that hang is reaped only by the 900s wall,
/// which kills the run and emits an empty result.json.  120s is generous
/// enough not to trip on a slow time-to-first-token or a long inter-token
/// gap, yet — even after the transient retry budget (~3 attempts, see
/// [`MAX_ATTEMPTS`]) — stays well under that wall, so a truly stalled
/// stream surfaces as a retryable transport error instead of hanging.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProviderKind {
    Anthropic,
    Openai,
    Openrouter,
    Deepseek,
}

impl ProviderKind {
    /// `(label, default_model, key_env)` for this provider.
    pub fn info(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Anthropic => ("anthropic", "claude-opus-4", "ANTHROPIC_API_KEY"),
            Self::Openai => ("openai", "gpt-5.5", "OPENAI_API_KEY"),
            Self::Openrouter => (
                "openrouter",
                "anthropic/claude-opus-4",
                "OPENROUTER_API_KEY",
            ),
            Self::Deepseek => ("deepseek", "deepseek-chat", "DEEPSEEK_API_KEY"),
        }
    }

    /// Base URL for providers that need a custom endpoint.
    /// Must end with `/` — genai uses `Url::join` to append the
    /// service path (e.g. `chat/completions`).
    pub fn endpoint(self) -> Option<&'static str> {
        match self {
            Self::Openrouter => Some("https://openrouter.ai/api/v1/"),
            _ => None,
        }
    }

    /// The genai adapter this provider should use by default when no
    /// custom endpoint overrides the service target.
    pub fn default_adapter(self) -> AdapterKind {
        match self {
            Self::Anthropic => AdapterKind::Anthropic,
            Self::Openai => AdapterKind::OpenAIResp,
            Self::Openrouter => AdapterKind::OpenAI,
            Self::Deepseek => AdapterKind::DeepSeek,
        }
    }
}

/// An unusual provider declared in `config.ral`: a custom endpoint exarch
/// has no built-in knowledge of. It carries the same four facts the rest of
/// the code reads off a famous [`ProviderKind`] — label, key env var,
/// endpoint, wire adapter — but as owned, runtime data rather than the
/// `'static` table baked into the enum. Slice 3 of the provider-config ADR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomProvider {
    /// The provider's display label and stable identity (the config map key).
    pub label: String,
    /// The environment variable its API key is read from.
    pub key_env: String,
    /// The base URL genai's [`ServiceTargetResolver`] points traffic at.
    /// Always a custom endpoint — that a custom provider has one is the
    /// whole reason it needs a config entry.
    pub endpoint: String,
    /// The wire protocol, one of the three the config admits, already mapped
    /// onto genai's `AdapterKind` at decode time.
    pub adapter: AdapterKind,
}

/// A provider identity: a famous [`ProviderKind`] or a custom-declared one.
///
/// This is the single abstraction credential resolution, model listing, and
/// transport building consume, so a custom provider flows through exactly the
/// same machinery as a famous one. Each downstream call site reads the four
/// facts it needs through [`Self::label`] / [`Self::key_env`] /
/// [`Self::endpoint`] / [`Self::adapter`] and never matches on the
/// famous-vs-custom distinction. The `Custom` arm holds an [`Arc`] so the id
/// stays cheap to clone and to use as a map key.
///
/// Identity (`Eq`/`Ord`/`Hash`) is keyed on the label alone — a provider's
/// label is its unique key in the credential store, the model catalog, and
/// the picker — so the wrapping `AdapterKind` (no `Ord`) need not order.
#[derive(Clone, Debug)]
pub enum ProviderId {
    /// A built-in famous provider.
    Famous(ProviderKind),
    /// An unusual provider declared in `config.ral`.
    Custom(Arc<CustomProvider>),
}

impl ProviderId {
    /// The stable label — `ProviderKind::info().0` for a famous provider, the
    /// config map key for a custom one.
    pub fn label(&self) -> &str {
        match self {
            Self::Famous(kind) => kind.info().0,
            Self::Custom(c) => &c.label,
        }
    }

    /// The environment variable this provider's API key is read from.
    pub fn key_env(&self) -> &str {
        match self {
            Self::Famous(kind) => kind.info().2,
            Self::Custom(c) => &c.key_env,
        }
    }

    /// The base URL for a provider that needs a custom endpoint, or `None`
    /// when the native adapter's default target is used. A custom provider
    /// always has one.
    pub fn endpoint(&self) -> Option<&str> {
        match self {
            Self::Famous(kind) => kind.endpoint(),
            Self::Custom(c) => Some(&c.endpoint),
        }
    }

    /// The genai adapter this provider speaks by default. For a famous
    /// provider this is the per-kind default; a custom provider names its
    /// wire protocol in the config and carries the resolved adapter.
    pub fn default_adapter(&self) -> AdapterKind {
        match self {
            Self::Famous(kind) => kind.default_adapter(),
            Self::Custom(c) => c.adapter,
        }
    }

    /// The famous kind, when this is a built-in provider — the few sites that
    /// genuinely need the enum (the OpenAI per-model adapter refinement, the
    /// persisted-selection round-trip) ask for it; everything else reads the
    /// four facts above.
    pub fn famous(&self) -> Option<ProviderKind> {
        match self {
            Self::Famous(kind) => Some(*kind),
            Self::Custom(_) => None,
        }
    }
}

impl PartialEq for ProviderId {
    fn eq(&self, other: &Self) -> bool {
        self.label() == other.label()
    }
}

impl Eq for ProviderId {}

impl std::hash::Hash for ProviderId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.label().hash(state);
    }
}

impl PartialOrd for ProviderId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ProviderId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.label().cmp(other.label())
    }
}

#[derive(Default, Clone, Copy)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    /// Tokens written into the prompt cache this turn.  `None` means the
    /// provider does not surface this number at all (OpenAI-family
    /// endpoints — caching is automatic and writes aren't on the wire);
    /// `Some(0)` is a real measured zero (Anthropic on a cold turn).
    pub cache_creation: Option<u64>,
    /// Tokens read as cache hits this turn.  Same `None` vs `Some(0)`
    /// distinction as [`Self::cache_creation`].
    pub cache_read: Option<u64>,
    pub dollars: f64,
    /// Whether this turn's cost is off the meter: a ChatGPT plan login is
    /// a flat subscription, so the per-token dollar figure is meaningless
    /// and the cost slot reads "subscription" rather than a price.
    /// Defaults to `false` — an API-key turn is metered.
    pub unmetered: bool,
}

/// `None` is "the provider didn't report it"; once any turn has seen the
/// field, the accumulator keeps the `Some` flavour rather than collapsing
/// back to `None`.
fn add_opt_u64(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x + y),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, rhs: Self) {
        self.input += rhs.input;
        self.output += rhs.output;
        self.cache_creation = add_opt_u64(self.cache_creation, rhs.cache_creation);
        self.cache_read = add_opt_u64(self.cache_read, rhs.cache_read);
        self.dollars += rhs.dollars;
        self.unmetered = self.unmetered || rhs.unmetered;
    }
}

/// Humanise a token count: `≥10k` collapses to one decimal place
/// (`4017.4k`), smaller counts print in full.  The single rule the usage
/// line uses for every field — shared by [`Usage::parts`] so the plain
/// [`Display`] and the TUI's styled renderer cannot drift.
pub fn humanize_tokens(n: u64) -> String {
    if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// The humanised pieces of a usage line.  One source of truth for the
/// usage rendering's *content* and layout decisions; `Display` joins the
/// pieces as plain text and `tui::line::usage_text` styles them, so the
/// two surfaces (X9) never disagree on what to show.  A cache field that
/// the provider did not report renders `—`; an unreported / zero cost
/// renders `—`; the cache segment is omitted entirely when neither side
/// is a measured positive.
pub struct UsageParts {
    pub input: String,
    pub output: String,
    /// `Some((write, read))` when the cache segment is shown.
    pub cache: Option<(String, String)>,
    pub cost: String,
}

impl Usage {
    pub fn parts(&self) -> UsageParts {
        let field = |v: Option<u64>| match v {
            Some(n) => humanize_tokens(n),
            None => "—".into(),
        };
        let show_cache = matches!(self.cache_creation, Some(n) if n > 0)
            || matches!(self.cache_read, Some(n) if n > 0);
        UsageParts {
            input: humanize_tokens(self.input),
            output: humanize_tokens(self.output),
            cache: show_cache.then(|| (field(self.cache_creation), field(self.cache_read))),
            cost: if self.unmetered {
                "subscription".into()
            } else if self.dollars > 0.0 {
                format!("${:.4}", self.dollars)
            } else {
                "—".into()
            },
        }
    }
}

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = self.parts();
        write!(f, "total {} in / {} out", p.input, p.output)?;
        if let Some((wr, rd)) = &p.cache {
            write!(f, " [{wr} wr/{rd} rd]")?;
        }
        write!(f, " · {}", p.cost)
    }
}

/// One streamed assistant response.
pub struct StepOut {
    pub assistant_message: ChatMessage,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    /// Provider-normalised stop reason for this turn.  `None` means the
    /// adapter did not surface one (most non-Anthropic adapters do).
    /// Critical signal: `Some(StopReason::MaxTokens(_))` means the
    /// assistant text — and any tool call the model was about to emit —
    /// was truncated by [`Provider`]'s output cap.  The session loop
    /// must surface this rather than treating "no tool call" as
    /// "the model is done".
    pub stop_reason: Option<StopReason>,
}

/// One compaction summary response.
pub struct SummaryOut {
    pub summary: String,
    pub usage: Usage,
}

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
/// `serde_json::Value` body.  The `Display`/substring fallback survives
/// only for variants the typed API exposes no status on (stream parse
/// failures, auth/config errors, a residual `reqwest::Error` whose
/// classification rests on transport predicates).
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
    /// The turn finished with a non-success `StopReason` — most
    /// commonly `max_tokens`, where the provider truncated the response
    /// before the model could emit a tool call or finish its text.
    /// Raised by [`crate::session::Session::apply`] **after** appending
    /// the partial assistant message, so re-prompting with `continue`
    /// preserves the partial work as transcript context.
    Truncated {
        /// The provider's normalised reason as a string (e.g.
        /// `"max_tokens"`, `"length"`, `"content_filter"`).
        reason: String,
    },
    /// Anything else: rendered raw so the user has the original error
    /// text to work with.
    Other(String),
}

impl ProviderError {
    /// Classify a typed genai error into a provider-boundary failure.
    ///
    /// The retryability of the result is decided structurally: a
    /// [`StatusCode`] recovered from the error's typed variants drives
    /// the 429 / 5xx / 4xx split (see [`status_of`]), a `retry-after`
    /// header is honoured when the variant carries one, and a
    /// `ChatResponse` JSON error frame is read from its value.  Only when
    /// no status can be recovered does the classifier fall back to the
    /// `Display` string and the transport-transient heuristic.
    pub fn from_genai(err: &genai::Error, model: &str) -> Self {
        if let Some((status, headers, body)) = status_of(err) {
            let msg = err.to_string();
            if status == StatusCode::TOO_MANY_REQUESTS {
                let retry_after = headers
                    .and_then(retry_after_header)
                    .or_else(|| parse_retry_after(&msg));
                return ProviderError::RateLimited {
                    retry_after,
                    cause: msg,
                    body,
                };
            }
            if status.is_server_error() {
                return ProviderError::Transient {
                    cause: msg,
                    attempts: 1,
                    body,
                };
            }
            // Any remaining non-success status (4xx, redirects) is a
            // request the user must change — not retryable.
            return ProviderError::Api {
                status: Some(status.as_u16()),
                model: model.to_string(),
                message: msg.clone(),
                url: extract_url(&msg),
                body,
            };
        }

        // A genuine transport fault (connect / timeout / reset) reaches us
        // as a `reqwest::Error` boxed inside `WebStream` — no HTTP status
        // was ever received.  This is the canonical retryable failure.
        if is_transport_transient(err) {
            return ProviderError::Transient {
                cause: err.to_string(),
                attempts: 1,
                body: None,
            };
        }

        // Variants the typed API exposes no status on: fall back to the
        // `Display` string and the transient-keyword heuristic for the
        // residual stream/network shapes that carry no structured status.
        let msg = err.to_string();
        let lower = msg.to_lowercase();
        if lower.contains("web stream error")
            || lower.contains("sending request")
            || lower.contains("connection")
            || lower.contains("timeout")
            || lower.contains("timed out")
            || lower.contains("reset")
        {
            return ProviderError::Transient {
                cause: msg,
                attempts: 1,
                body: None,
            };
        }

        ProviderError::Other(msg)
    }
}

/// Recover the HTTP status, its response headers (when carried), and the
/// parsed JSON error body from a genai error's typed variants, covering
/// all three paths a non-2xx status reaches us by:
///
/// * `HttpError` — the raw HTTP-level error (streaming initial response);
///   its `body: String` is parsed best-effort.
/// * `WebModelCall(ResponseFailedStatus)` — the non-streamed `exec_chat`
///   path (the compaction summary); the `headers` carry `retry-after` and
///   the `body: String` is parsed best-effort.
/// * `WebStream { error, .. }` — the streaming path boxes the typed
///   `HttpError` as the `BoxError` cause; downcast to recover its status
///   and parse its `body: String` best-effort.
/// * `ChatResponse { body, .. }` — a mid-stream JSON error frame whose
///   status lives in `body["error"]["code"]` / `body["code"]`; the body is
///   already a `serde_json::Value`, so it is cloned.
///
/// A non-JSON body (e.g. an HTML 5xx page) yields `None` for the body and
/// the caller falls back to the textual `cause`.
fn status_of(
    err: &genai::Error,
) -> Option<(StatusCode, Option<&HeaderMap>, Option<serde_json::Value>)> {
    match err {
        genai::Error::HttpError { status, body, .. } => {
            Some((*status, None, parse_json_body(body)))
        }
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => match webc_error {
            genai::webc::Error::ResponseFailedStatus {
                status,
                headers,
                body,
            } => Some((*status, Some(headers), parse_json_body(body))),
            _ => None,
        },
        genai::Error::WebStream { error, .. } => match error.downcast_ref::<genai::Error>() {
            Some(genai::Error::HttpError { status, body, .. }) => {
                Some((*status, None, parse_json_body(body)))
            }
            _ => None,
        },
        genai::Error::ChatResponse { body, .. } => {
            let code = json_status_code(body)?;
            StatusCode::from_u16(code)
                .ok()
                .map(|s| (s, None, Some(body.clone())))
        }
        _ => None,
    }
}

/// Parse a provider error body string as JSON, best-effort.  A non-JSON
/// body (e.g. an HTML 5xx page) yields `None` so the caller falls back to
/// the textual `cause`.
fn parse_json_body(s: &str) -> Option<serde_json::Value> {
    serde_json::from_str(s).ok()
}

/// Read a status code from a provider JSON error body, accepting both the
/// nested `{"error":{"code":NNN}}` and the flat `{"code":NNN}` shapes.
fn json_status_code(body: &serde_json::Value) -> Option<u16> {
    body.get("error")
        .and_then(|e| e.get("code"))
        .or_else(|| body.get("code"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|c| u16::try_from(c).ok())
}

/// Whether `err` is a transport-level fault (connect / timeout / request
/// failure) with no HTTP status — the streaming path boxes such faults as
/// a `reqwest::Error` inside `WebStream`.  These are retryable.
fn is_transport_transient(err: &genai::Error) -> bool {
    let genai::Error::WebStream { error, .. } = err else {
        return false;
    };
    error
        .downcast_ref::<reqwest::Error>()
        .is_some_and(|e| e.is_connect() || e.is_timeout() || e.is_request())
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

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::Cancelled(where_) => write!(f, "cancelled {where_}"),
            ProviderError::Transient {
                cause, attempts, ..
            } => {
                if *attempts > 1 {
                    write!(f, "transient error after {attempts} attempts: {cause}")
                } else {
                    write!(f, "transient error: {cause}")
                }
            }
            ProviderError::RateLimited { cause, .. } => {
                write!(f, "rate limited: {cause}")
            }
            ProviderError::Api {
                status,
                model,
                message,
                ..
            } => match status {
                Some(s) => write!(f, "api error {s} ({model}): {message}"),
                None => write!(f, "api error ({model}): {message}"),
            },
            ProviderError::Truncated { reason } => write!(
                f,
                "truncated by provider (stop_reason={reason}): output cap was hit \
                 before the model finished — raise --max-tokens or split the turn"
            ),
            ProviderError::Other(s) => f.write_str(s),
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
        .take_while(|c| c.is_ascii_digit())
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

/// Per-error retry budget and backoff ceiling.  Rate limits get a
/// larger budget and a higher delay ceiling than generic transient
/// failures; every other variant uses the transient defaults (and is
/// only ever reached for the two retryable variants in any case).
fn retry_limits(err: &ProviderError) -> (u32, u64) {
    match err {
        ProviderError::RateLimited { .. } => (RATE_LIMIT_MAX_ATTEMPTS, RATE_LIMIT_MAX_DELAY_MS),
        _ => (MAX_ATTEMPTS, MAX_DELAY_MS),
    }
}

/// Sleep for `attempt`'s backoff window, honouring an explicit
/// `retry_after` from the server if present.  Exponential backoff:
/// `BASE_DELAY_MS * 2^(attempt-1)`, capped at `max_delay_ms` (the
/// per-error ceiling from [`retry_limits`]).
async fn backoff_sleep(attempt: u32, retry_after: Option<Duration>, max_delay_ms: u64) {
    if let Some(d) = retry_after {
        let capped = d.min(Duration::from_millis(max_delay_ms));
        tokio::time::sleep(capped).await;
        return;
    }
    let shift = attempt.saturating_sub(1).min(16);
    let ms = BASE_DELAY_MS
        .saturating_mul(1u64 << shift)
        .min(max_delay_ms);
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// One attempt's outcome, as the retry driver sees it.
enum Attempt<T> {
    /// Success — return it.
    Done(T),
    /// Failed; the driver retries when the error is transient/rate-limited
    /// and budget remains (see [`retry_limits`]), otherwise surfaces it.
    Failed(ProviderError),
    /// Failed and must not be retried regardless of class — the stream
    /// already committed partial output to the screen, so a re-issue would
    /// double-render.  Surfaced immediately.
    Committed(ProviderError),
}

/// Drive `one` under the transient/rate-limit retry policy: bounded,
/// per-error exponential backoff (see [`retry_limits`] / [`backoff_sleep`]),
/// cancellation-aware.  `cancel_site` labels where a cancel that lands
/// between attempts is attributed.  `one` is invoked with the 1-based
/// attempt number and performs exactly one request.
async fn retry_with_backoff<T>(
    cancel_site: &'static str,
    mut one: impl AsyncFnMut(u32) -> Attempt<T>,
) -> Result<T, ProviderError> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        if cancel::is_set() {
            return Err(ProviderError::Cancelled(cancel_site));
        }
        let err = match one(attempt).await {
            Attempt::Done(v) => return Ok(v),
            Attempt::Committed(e) => return Err(stamp_attempts(e, attempt)),
            Attempt::Failed(e @ ProviderError::Cancelled(_)) => return Err(e),
            Attempt::Failed(e) => e,
        };
        if !matches!(
            err,
            ProviderError::Transient { .. } | ProviderError::RateLimited { .. }
        ) {
            return Err(stamp_attempts(err, attempt));
        }
        let (max_attempts, max_delay_ms) = retry_limits(&err);
        if attempt >= max_attempts {
            return Err(stamp_attempts(err, attempt));
        }
        let retry_after = match &err {
            ProviderError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        };
        tokio::select! {
            biased;
            _ = wait_for_cancel() => return Err(ProviderError::Cancelled(cancel_site)),
            _ = backoff_sleep(attempt, retry_after, max_delay_ms) => {}
        }
    }
}

pub struct Provider {
    backend: Backend,
    model: String,
    /// User-supplied `--max-tokens` override.  `None` means we don't
    /// set the option at all and genai applies its per-model default
    /// (the correct path for Opus, Sonnet, Haiku and the other adapters
    /// that ship their own resolution tables).
    max_tokens_override: Option<u32>,
}

/// The transport behind a [`Provider`].  The live backend owns the
/// genai client and tokio runtime; the scripted backend (test-only)
/// replays canned [`StepOut`] / [`SummaryOut`] outcomes so the session
/// turn driver can be exercised end-to-end without a network.
enum Backend {
    Live(Live),
    Scripted(scripted::Script),
}

/// Live genai transport state.
struct Live {
    client: Client,
    runtime: tokio::runtime::Runtime,
    /// Stable per-process identifier sent as OpenAI's `prompt_cache_key`
    /// on every request.  OpenAI uses it as a routing hint so all calls
    /// from one exarch process land on the same backend shard, raising
    /// cache-hit rate.  Anthropic and other providers ignore unknown
    /// fields, so it is harmless elsewhere.
    cache_key: String,
    /// The shared login cell when this provider authenticates off a ChatGPT
    /// plan; `None` for an API-key provider.  A turn refreshes through it
    /// before a request when the access token is near expiry.
    token_cell: Option<Arc<Mutex<oauth::OAuthToken>>>,
    /// The genai adapter this provider speaks, fixed at build time.  It
    /// decides where the system prompt rides on the wire: the Responses
    /// API takes it as top-level `instructions`, every other adapter as a
    /// cache-marked system message (see [`build_cached_request`]).
    adapter: AdapterKind,
}

fn fresh_cache_key() -> String {
    format!("exarch-{}", std::process::id())
}

fn make_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio multi-thread runtime")
}

fn adapter_for_provider_model(id: &ProviderId, model: &str) -> AdapterKind {
    match id {
        ProviderId::Famous(ProviderKind::Openai) => {
            match AdapterKind::from_model(model).unwrap_or(AdapterKind::OpenAIResp) {
                adapter @ (AdapterKind::OpenAI | AdapterKind::OpenAIResp) => adapter,
                _ => AdapterKind::OpenAIResp,
            }
        }
        _ => id.default_adapter(),
    }
}

/// Build the genai [`Client`] for a provider and model, binding the
/// resolved `cred`. This is the live *transport* client; model listing
/// ([`crate::models::LiveSource`]) builds its own client for genai's
/// catalog call, drawing its endpoint and key from the same [`ProviderKind`]
/// source but using the provider's default adapter — listing has no
/// per-model context to refine on.
///
/// An [`Credential::OAuth`] login branches to [`build_oauth_client`]. For an
/// API key the adapter is always the provider's native one
/// ([`adapter_for_provider_model`]) — it decides the wire format, so an
/// Anthropic provider speaks Anthropic even at a custom base URL. A
/// provider with a custom `endpoint` (OpenRouter) redirects the *service
/// target* through a [`ServiceTargetResolver`] that keeps the native
/// adapter and binds the key; with no endpoint the native adapter's
/// default target is used, gated by an [`AuthResolver`] that hands the key
/// only to that adapter.
///
/// genai's model-name heuristic falls back to local Ollama for unknown
/// names; the provider is the authority instead, so a misspelled DeepSeek
/// model fails at DeepSeek rather than being silently sent to
/// `localhost:11434`.
fn build_client(id: &ProviderId, model: &str, cred: &Credential) -> (Client, AdapterKind) {
    let key = match cred {
        Credential::ApiKey(k) => k.clone(),
        // A ChatGPT login does not bind a bearer the adapter resolves
        // normally: it redirects the request to the Codex backend with its
        // own headers, so its client is built apart — and always speaks the
        // Responses adapter.
        Credential::OAuth(cell) => {
            return (build_oauth_client(cell.clone()), AdapterKind::OpenAIResp);
        }
    };
    let adapter = adapter_for_provider_model(id, model);
    let client = match id.endpoint() {
        Some(base_url) => {
            let endpoint = Endpoint::from_owned(base_url);
            let resolver = ServiceTargetResolver::from_resolver_fn(move |t: ServiceTarget| {
                Ok(ServiceTarget {
                    endpoint: endpoint.clone(),
                    auth: AuthData::from_single(key.clone()),
                    model: ModelIden::new(adapter, t.model.model_name),
                })
            });
            Client::builder()
                .with_reqwest(crate::tls::client())
                .with_service_target_resolver(resolver)
                .build()
        }
        None => {
            let auth = AuthResolver::from_resolver_fn(move |iden: ModelIden| {
                if iden.adapter_kind == adapter {
                    Ok(Some(AuthData::from_single(key.clone())))
                } else {
                    Ok(None)
                }
            });
            Client::builder()
                .with_reqwest(crate::tls::client())
                .with_adapter_kind(adapter)
                .with_auth_resolver(auth)
                .build()
        }
    };
    (client, adapter)
}

/// Build the genai client for a ChatGPT plan login. The OpenAI Responses
/// adapter renders the request body; an [`AuthResolver`] redirects every
/// request to the Codex backend with the login's bearer and account headers,
/// read live from `cell` so a token refreshed mid-session is picked up on the
/// next request without rebuilding the client.
fn build_oauth_client(cell: Arc<Mutex<oauth::OAuthToken>>) -> Client {
    let auth = AuthResolver::from_resolver_fn(move |iden: ModelIden| {
        if iden.adapter_kind == AdapterKind::OpenAIResp {
            let token = cell.lock().unwrap_or_else(|e| e.into_inner());
            Ok(Some(AuthData::RequestOverride {
                url: oauth::RESPONSES_URL.to_string(),
                headers: Headers::from(oauth::request_headers(&token)),
            }))
        } else {
            Ok(None)
        }
    });
    Client::builder()
        .with_reqwest(crate::tls::client())
        .with_adapter_kind(AdapterKind::OpenAIResp)
        .with_auth_resolver(auth)
        .build()
}

impl Provider {
    /// Build a live provider for `kind` + `model`, binding the resolved
    /// `cred`. The single live-transport construction path: `run` calls it
    /// for the starting selection, and a `/model` switch calls it again for
    /// the chosen provider+model over the same transcript.
    pub fn build(
        id: &ProviderId,
        model: String,
        cred: &Credential,
        max_tokens_override: Option<u32>,
    ) -> Self {
        let runtime = make_runtime();
        prime_pricing(&runtime);
        let token_cell = match cred {
            Credential::OAuth(cell) => Some(cell.clone()),
            Credential::ApiKey(_) => None,
        };
        let (client, adapter) = build_client(id, &model, cred);
        Self {
            backend: Backend::Live(Live {
                client,
                runtime,
                cache_key: fresh_cache_key(),
                token_cell,
                adapter,
            }),
            model,
            max_tokens_override,
        }
    }

    /// A test provider that replays scripted outcomes instead of
    /// talking to genai.  No tokio runtime, no network: `complete` and
    /// `summarize` pop their next outcome from `script` in order.  This
    /// is the seam the `tests/` harness drives [`crate::session::Session::apply`]
    /// through — see [`scripted::Script`].
    pub fn scripted(model: &str, script: scripted::Script) -> Self {
        Self {
            backend: Backend::Scripted(script),
            model: model.to_string(),
            max_tokens_override: None,
        }
    }

    /// The `--max-tokens` override, if the user passed one.  `None`
    /// means the banner should render "auto" — genai's adapter picks a
    /// per-model default at request time.
    pub fn max_tokens_override(&self) -> Option<u32> {
        self.max_tokens_override
    }

    /// The resolved model name, used by the banner and the capabilities
    /// lookup.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// True when this provider authenticates off a ChatGPT plan login
    /// rather than an API key — a flat subscription whose turns carry no
    /// per-token price. The login cell is present only for an OAuth
    /// credential; a scripted backend is never a subscription.
    pub fn is_subscription(&self) -> bool {
        matches!(&self.backend, Backend::Live(live) if live.token_cell.is_some())
    }

    /// Stream one assistant response for an already-rendered message list.
    ///
    /// Retries with bounded exponential backoff on `Transient` and
    /// `RateLimited` errors **only when zero tokens have streamed this
    /// attempt** — once any token has flowed to `on_text` the UI has
    /// committed to a partial render, and re-streaming the request
    /// would double tokens.  See the design doc's option (A).
    pub fn complete<F: FnMut(&str)>(
        &self,
        system: &str,
        messages: Vec<ChatMessage>,
        advertise_root_only: bool,
        on_text: &mut F,
    ) -> Result<StepOut, ProviderError> {
        match &self.backend {
            Backend::Live(live) => live.complete(
                &self.model,
                self.max_tokens_override,
                system,
                messages,
                advertise_root_only,
                on_text,
            ),
            Backend::Scripted(s) => s.complete(&self.model, on_text),
        }
    }

    /// Summarise already-rendered transcript messages.
    ///
    /// Retries unconditionally on Transient/RateLimited — there is no
    /// streamed side effect to worry about.
    pub fn summarize(
        &self,
        system: &str,
        messages: Vec<ChatMessage>,
    ) -> Result<SummaryOut, ProviderError> {
        match &self.backend {
            Backend::Live(live) => live.summarize(&self.model, system, messages),
            Backend::Scripted(s) => s.summarize(&self.model),
        }
    }
}

impl Live {
    /// Whether this backend's turns are billed per token. An API-key
    /// provider is metered; a ChatGPT plan login is a flat subscription
    /// and so unmetered.
    fn metered(&self) -> bool {
        self.token_cell.is_none()
    }

    /// Renew the ChatGPT access token when it is near expiry, so the request
    /// that follows authenticates with a live token.  A no-op for an API-key
    /// provider.  A refresh failure is logged and left to surface as the
    /// request's own auth error rather than aborting the turn here.
    fn refresh_if_stale(&self) {
        let Some(cell) = &self.token_cell else {
            return;
        };
        // Copy the token out only when a refresh is actually due — the common
        // case finds it fresh and returns under the lock without cloning.
        let current = {
            let token = cell.lock().unwrap_or_else(|e| e.into_inner());
            if !token.is_stale() {
                return;
            }
            token.clone()
        };
        match self.runtime.block_on(oauth::refresh(&current)) {
            Ok(fresh) => {
                let _ = oauth::save(&fresh);
                *cell.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
            }
            Err(e) => eprintln!("exarch: ChatGPT token refresh failed: {e}"),
        }
    }

    fn complete<F: FnMut(&str)>(
        &self,
        model: &str,
        max_tokens_override: Option<u32>,
        system: &str,
        messages: Vec<ChatMessage>,
        advertise_root_only: bool,
        on_text: &mut F,
    ) -> Result<StepOut, ProviderError> {
        self.refresh_if_stale();
        let req_template = build_cached_request(self.adapter, system, messages);
        let mut options = ChatOptions::default()
            .with_capture_usage(true)
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_capture_reasoning_content(true)
            .with_prompt_cache_key(&self.cache_key);
        // Only override genai's per-model default when the user
        // explicitly asked.  The pre-fix code pinned a 4k constant
        // here and silently truncated long Opus turns.
        if let Some(n) = max_tokens_override {
            options = options.with_max_tokens(n);
        }

        let end =
            self.runtime
                .block_on(retry_with_backoff("before request", async |_attempt| {
                    let mut req = req_template.clone();
                    req.tools = Some(tool_defs(advertise_root_only));
                    let mut seen_any_token = false;
                    let attempt_result: Result<StreamEnd, ProviderError> = async {
                        let mut resp = tokio::select! {
                            biased;
                            _ = wait_for_cancel() => {
                                return Err(ProviderError::Cancelled("before request"));
                            }
                            // A fresh `sleep` per select entry bounds connect +
                            // time-to-first-event; surfaced as a transient
                            // transport error so the retry budget re-issues it
                            // (no token has streamed yet).
                            _ = tokio::time::sleep(STREAM_IDLE_TIMEOUT) => {
                                return Err(ProviderError::Transient {
                                    cause: "stream idle: no response within timeout".into(),
                                    attempts: 1,
                                    body: None,
                                });
                            }
                            r = self.client.exec_chat_stream(model, req, Some(&options)) => {
                                r.map_err(|e| ProviderError::from_genai(&e, model))?
                            }
                        };
                        loop {
                            let event = tokio::select! {
                                biased;
                                _ = wait_for_cancel() => {
                                    return Err(ProviderError::Cancelled("mid-stream"));
                                }
                                // Re-armed each loop iteration, so it is a
                                // per-chunk idle timeout: a stream that stops
                                // emitting events surfaces as a transient error
                                // rather than blocking `next()` forever.
                                _ = tokio::time::sleep(STREAM_IDLE_TIMEOUT) => {
                                    return Err(ProviderError::Transient {
                                        cause: "stream idle: no event within timeout".into(),
                                        attempts: 1,
                                        body: None,
                                    });
                                }
                                ev = resp.stream.next() => match ev {
                                    Some(Ok(ev)) => ev,
                                    Some(Err(e)) => {
                                        return Err(ProviderError::from_genai(&e, model));
                                    }
                                    None => break,
                                }
                            };
                            match event {
                                ChatStreamEvent::Start => {}
                                ChatStreamEvent::Chunk(c) => {
                                    if !c.content.is_empty() {
                                        seen_any_token = true;
                                    }
                                    on_text(&c.content);
                                }
                                ChatStreamEvent::End(end) => return Ok(end),
                                // The remaining variants are captured in the
                                // `End` frame (`captured_reasoning_content`,
                                // the thought signatures and tool calls on the
                                // assistant message) and replayed from there by
                                // `step_out_from_end`, so no live action is
                                // needed here.  Matched explicitly rather than
                                // with a wildcard so a new genai stream variant
                                // fails the build instead of vanishing (X10).
                                ChatStreamEvent::ReasoningChunk(_)
                                | ChatStreamEvent::ThoughtSignatureChunk(_)
                                | ChatStreamEvent::ToolCallChunk(_) => {}
                            }
                        }
                        Err(ProviderError::Other(
                            "stream ended without End event".into(),
                        ))
                    }
                    .await;

                    match attempt_result {
                        Ok(end) => Attempt::Done(end),
                        // A cancel is surfaced as-is regardless of streamed tokens.
                        Err(e @ ProviderError::Cancelled(_)) => Attempt::Failed(e),
                        // Tokens already streamed: a re-issue would double-render.
                        Err(e) if seen_any_token => Attempt::Committed(e),
                        Err(e) => Attempt::Failed(e),
                    }
                }))?;

        Ok(step_out_from_end(model, end, self.metered()))
    }

    fn summarize(
        &self,
        model: &str,
        system: &str,
        mut messages: Vec<ChatMessage>,
    ) -> Result<SummaryOut, ProviderError> {
        self.refresh_if_stale();
        messages.push(ChatMessage::user(
            "Summarise the conversation so far in one paragraph: \
            the user's task, what has been tried, what worked, what state the \
            shell is in (cwd, env, defined names), and any open subtasks. \
            Return only the summary, no preamble.",
        ));
        let req_template = build_cached_request(self.adapter, system, messages);
        let options = ChatOptions::default()
            .with_max_tokens(1024)
            .with_prompt_cache_key(&self.cache_key);

        let resp =
            self.runtime
                .block_on(retry_with_backoff("during summary", async |_attempt| {
                    let req = req_template.clone();
                    let r = tokio::select! {
                        biased;
                        _ = wait_for_cancel() => {
                            return Attempt::Failed(ProviderError::Cancelled("during summary"));
                        }
                        // `summarize` is non-streaming, so there are no
                        // incremental events to idle between: the same budget
                        // bounds the whole `exec_chat` request.  `tls::client`
                        // sets no timeout, so without this a stalled connection
                        // hangs here exactly as it did in `complete` — reaped
                        // only by the 900s harness wall.  No partial output is
                        // ever rendered, so it is `Failed` (retryable), never
                        // `Committed`.
                        _ = tokio::time::sleep(STREAM_IDLE_TIMEOUT) => {
                            return Attempt::Failed(ProviderError::Transient {
                                cause: "summary request: no response within timeout".into(),
                                attempts: 1,
                                body: None,
                            });
                        }
                        r = self.client.exec_chat(model, req, Some(&options)) => r,
                    };
                    match r {
                        Ok(resp) => Attempt::Done(resp),
                        Err(e) => Attempt::Failed(ProviderError::from_genai(&e, model)),
                    }
                }))?;

        // A summary that itself hit the 1024-token cap is incomplete;
        // replacing the whole history with it would silently drop
        // everything past the cut-off.  Surface it as `Truncated` so
        // `Session::compact` keeps the un-summarised history rather than
        // committing a half summary (X10).
        if matches!(resp.stop_reason, Some(StopReason::MaxTokens(_))) {
            return Err(ProviderError::Truncated {
                reason: "summary exceeded the compaction token budget".into(),
            });
        }
        let text = resp.first_text().unwrap_or("").to_string();
        Ok(SummaryOut {
            summary: text,
            usage: usage_from(model, &resp.usage, self.metered()),
        })
    }
}

/// Stamp the final attempt count onto a Transient/RateLimited error
/// before surfacing it.  Non-retryable variants pass through unchanged
/// — the caller cares about retry counts only for the kinds that
/// actually loop.
fn stamp_attempts(err: ProviderError, attempts: u32) -> ProviderError {
    match err {
        ProviderError::Transient { cause, body, .. } => ProviderError::Transient {
            cause,
            attempts,
            body,
        },
        other => other,
    }
}

/// Project a captured [`StreamEnd`] into a [`StepOut`], preserving the
/// provider's reasoning trace on the assistant message so it round-trips
/// on the next request.  DeepSeek's thinking mode *requires*
/// `reasoning_content` to be echoed back on any assistant turn that
/// precedes tool results — dropping it returns a 400 ("the
/// `reasoning_content` in the thinking mode must be passed back to the
/// API").  genai's OpenAI adapter emits the field only when the message
/// carries a `ReasoningContent` part, which `with_reasoning_content`
/// attaches; `with_reasoning_content(None)` is a no-op, so providers that
/// surface no reasoning are unaffected.  `captured_into_tool_calls`
/// consumes `end`, so the other fields are read first.
fn step_out_from_end(model: &str, end: StreamEnd, metered: bool) -> StepOut {
    let raw_usage = end.captured_usage.clone().unwrap_or_default();
    let stop_reason = end.captured_stop_reason.clone();
    let reasoning = end.captured_reasoning_content.clone();
    let content = end.captured_content.clone().unwrap_or_default();
    let tool_calls = end.captured_into_tool_calls().unwrap_or_default();
    StepOut {
        assistant_message: ChatMessage::assistant(content).with_reasoning_content(reasoning),
        tool_calls,
        usage: usage_from(model, &raw_usage, metered),
        stop_reason,
    }
}

// ── helpers ────────────────────────────────────────────────────────

/// Build a [`ChatRequest`] carrying the system prompt with two
/// `cache_control: ephemeral` breakpoints: one on the system block
/// (caches `tools + system`, which never changes across a session) and
/// one on the final message (caches the growing transcript so the inner
/// tool-use loop pays cache_read rate on the prefix and cache_creation
/// only on the delta).
///
/// Anthropic honours these breakpoints directly.  OpenAI auto-caches any
/// prompt ≥ 1024 tokens with no client opt-in; the per-request lever that
/// actually matters there is `prompt_cache_key` (set in [`ChatOptions`]
/// above, derived from [`Provider::cache_key`]), which routes repeat
/// calls to the same backend shard.  Other providers ignore both.
///
/// The Responses API ([`AdapterKind::OpenAIResp`]) is the exception: its
/// system prompt is the top-level `instructions` field, which genai
/// sources from [`ChatRequest::system`].  A `ChatRole::System` message
/// would instead land in `input`, leaving `instructions` empty — which the
/// Codex backend rejects ("Instructions are required").  That adapter
/// caches off `prompt_cache_key` and ignores message-level cache_control,
/// so routing the system prompt to the field costs no caching.
fn build_cached_request(
    adapter: AdapterKind,
    system: &str,
    mut messages: Vec<ChatMessage>,
) -> ChatRequest {
    if let Some(last) = messages.last_mut() {
        last.options = Some(MessageOptions::from(CacheControl::Ephemeral));
    }
    if adapter == AdapterKind::OpenAIResp {
        return ChatRequest::new(messages).with_system(system);
    }
    let mut all = Vec::with_capacity(messages.len() + 1);
    all.push(
        ChatMessage::system(system.to_string())
            .with_options(MessageOptions::from(CacheControl::Ephemeral)),
    );
    all.extend(messages);
    ChatRequest::new(all)
}

fn tool_defs(advertise_root_only: bool) -> Vec<Tool> {
    crate::tools::registry()
        .iter()
        .filter(|t| advertise_root_only || !t.root_only())
        .map(|t| {
            Tool::new(t.name())
                .with_description(t.desc())
                .with_schema(t.schema().clone())
        })
        .collect()
}

async fn wait_for_cancel() {
    while !cancel::is_set() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Pre-fetch the pricing catalog on Provider construction so the very
/// first turn renders a real dollar figure instead of `—`.  Every
/// provider relies on the same catalog (see the module-level docstring
/// in [`crate::pricing`]), so the fetch fires unconditionally.  Blocks
/// the constructor for one HTTP round-trip; failures are absorbed
/// inside [`crate::pricing::ensure_loaded`] so an offline start still produces
/// a working Provider, with cost rendered as `—`.
fn prime_pricing(runtime: &tokio::runtime::Runtime) {
    runtime.block_on(crate::pricing::ensure_loaded());
}

/// Extract our [`Usage`] from genai's raw usage record, applying the
/// model's price card.  Used by both [`Provider::complete`] and
/// [`Provider::summarize`] so the compaction call is accounted for
/// identically — same cache_read/cache_write attribution, same dollars.
/// A catalog miss bills $0 (rendered as `—`), matching the offline-
/// start behaviour of [`prime_pricing`].  When `metered` is false the
/// turn is a flat-subscription one: the price card is not consulted and
/// the usage is marked unmetered, so the cost slot reads "subscription".
fn usage_from(model: &str, raw: &genai::chat::Usage, metered: bool) -> Usage {
    let input = raw.prompt_tokens.unwrap_or(0) as u64;
    let output = raw.completion_tokens.unwrap_or(0) as u64;
    // Preserve the `None` vs `Some(0)` distinction: OpenAI-family
    // adapters leave `cache_creation_tokens` unset because the upstream
    // API does not report writes, and the renderer needs that signal so
    // it can show `—` instead of a misleading `0 wr`.
    let cache_creation = raw
        .prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cache_creation_tokens)
        .map(|n| n as u64);
    let cache_read = raw
        .prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cached_tokens)
        .map(|n| n as u64);
    let dollars = if metered {
        crate::pricing::lookup(model)
            .map(|p| {
                p.dollars(
                    input,
                    output,
                    cache_creation.unwrap_or(0),
                    cache_read.unwrap_or(0),
                )
            })
            .unwrap_or(0.0)
    } else {
        0.0
    };
    Usage {
        input,
        output,
        cache_creation,
        cache_read,
        dollars,
        unmetered: !metered,
    }
}

/// Scripted provider backend used by the `tests/` harness to drive
/// [`crate::session::Session::apply`] end-to-end with no network.  A
/// [`Script`] is a queue of canned `complete` replies (each optionally
/// streaming some text first) and a queue of `summarize` replies,
/// consumed in order.  Running past the end of either queue panics — a
/// test that under-scripts its turn should fail loudly, not hang on a
/// real request.
pub mod scripted {
    use super::{ProviderError, StepOut, SummaryOut, Usage};
    use genai::chat::{ChatMessage, StopReason, ToolCall};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// One scripted `complete` reply: the text to stream to `on_text`
    /// (before the step is projected) and the outcome the session sees.
    pub struct Reply {
        text: String,
        outcome: Result<StepOut, ProviderError>,
    }

    impl Reply {
        /// A plain assistant turn with `text` and no tool calls — the
        /// model is done.  Streams `text` to `on_text` and projects an
        /// assistant message carrying it.
        pub fn text(text: &str) -> Self {
            Self {
                text: text.to_string(),
                outcome: Ok(StepOut {
                    assistant_message: ChatMessage::assistant(text.to_string()),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    stop_reason: Some(StopReason::Completed("end_turn".into())),
                }),
            }
        }

        /// An assistant turn that requests tool calls.  No text streams;
        /// the session dispatches the calls and loops back for the next
        /// scripted reply.
        pub fn tool_calls(calls: Vec<ToolCall>) -> Self {
            let parts = calls
                .iter()
                .cloned()
                .map(genai::chat::ContentPart::ToolCall)
                .collect::<Vec<_>>();
            Self {
                text: String::new(),
                outcome: Ok(StepOut {
                    assistant_message: ChatMessage::assistant(parts),
                    tool_calls: calls,
                    usage: Usage::default(),
                    stop_reason: Some(StopReason::ToolCall("tool_use".into())),
                }),
            }
        }

        /// A truncated turn (`max_tokens`) carrying `calls` the model was
        /// mid-emitting — the X6 shape.
        pub fn truncated_with_tool_calls(calls: Vec<ToolCall>) -> Self {
            let parts = calls
                .iter()
                .cloned()
                .map(genai::chat::ContentPart::ToolCall)
                .collect::<Vec<_>>();
            Self {
                text: String::new(),
                outcome: Ok(StepOut {
                    assistant_message: ChatMessage::assistant(parts),
                    tool_calls: calls,
                    usage: Usage::default(),
                    stop_reason: Some(StopReason::MaxTokens("max_tokens".into())),
                }),
            }
        }

        /// An empty assistant turn — no text, no tool calls, end_turn —
        /// the X7 shape.  Mirrors the live path's
        /// `captured_content.unwrap_or_default()`: a zero-part
        /// `MessageContent`, which genai's Anthropic adapter serialises as
        /// `"content": []` and strict backends reject once a later message
        /// makes it non-final.
        pub fn empty() -> Self {
            Self {
                text: String::new(),
                outcome: Ok(StepOut {
                    assistant_message: ChatMessage::assistant(
                        genai::chat::MessageContent::default(),
                    ),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    stop_reason: Some(StopReason::Completed("end_turn".into())),
                }),
            }
        }

        /// A reply that surfaces `err` instead of an assistant message.
        pub fn error(err: ProviderError) -> Self {
            Self {
                text: String::new(),
                outcome: Err(err),
            }
        }

        /// Attach a usage delta to a reply (for compaction-threshold
        /// tests, which need history bytes to cross the cap).
        pub fn with_usage(mut self, usage: Usage) -> Self {
            if let Ok(step) = &mut self.outcome {
                step.usage = usage;
            }
            self
        }
    }

    /// A scripted run: `completes` are popped one per [`Provider::complete`]
    /// call, `summaries` one per [`Provider::summarize`] call.  Build with
    /// [`Script::new`] and chain [`Script::then`] / [`Script::then_summary`].
    ///
    /// [`Provider::complete`]: super::Provider::complete
    /// [`Provider::summarize`]: super::Provider::summarize
    pub struct Script {
        completes: Mutex<VecDeque<Reply>>,
        summaries: Mutex<VecDeque<Result<SummaryOut, ProviderError>>>,
    }

    impl Script {
        pub fn new() -> Self {
            Self {
                completes: Mutex::new(VecDeque::new()),
                summaries: Mutex::new(VecDeque::new()),
            }
        }

        /// Append a `complete` reply.
        pub fn then(self, reply: Reply) -> Self {
            self.completes.lock().unwrap().push_back(reply);
            self
        }

        /// Append a `summarize` reply.
        pub fn then_summary(self, summary: &str) -> Self {
            self.summaries.lock().unwrap().push_back(Ok(SummaryOut {
                summary: summary.to_string(),
                usage: Usage::default(),
            }));
            self
        }

        /// Append a `summarize` reply that was itself truncated by the
        /// compaction token budget — the X10 shape the Live backend
        /// surfaces as `Truncated` so `Session::compact` keeps the
        /// un-summarised history rather than committing a half summary.
        pub fn then_summary_truncated(self) -> Self {
            self.summaries
                .lock()
                .unwrap()
                .push_back(Err(ProviderError::Truncated {
                    reason: "summary exceeded the compaction token budget".into(),
                }));
            self
        }

        pub(super) fn complete<F: FnMut(&str)>(
            &self,
            _model: &str,
            on_text: &mut F,
        ) -> Result<StepOut, ProviderError> {
            let reply = self
                .completes
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted provider ran out of `complete` replies");
            if !reply.text.is_empty() {
                on_text(&reply.text);
            }
            reply.outcome
        }

        pub(super) fn summarize(&self, _model: &str) -> Result<SummaryOut, ProviderError> {
            self.summaries
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted provider ran out of `summarize` replies")
        }
    }

    impl Default for Script {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::ChatRole;
    use genai::chat::{ContentPart, MessageContent};
    use reqwest::header::HeaderValue;

    /// A `ModelIden` for the typed-error fixtures below.  The classifier
    /// never reads it, so the adapter kind is arbitrary.
    fn iden(model: &str) -> ModelIden {
        ModelIden::new(AdapterKind::Anthropic, model)
    }

    /// The non-streamed (`exec_chat`) failure shape: a non-2xx status
    /// surfaces as `WebModelCall(ResponseFailedStatus { status, headers })`
    /// — the path the compaction summary takes.  This is what
    /// [`Live::summarize`] hands to `from_genai`.
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

    #[test]
    fn deepseek_provider_binds_deepseek_adapter_for_bare_provider_name() {
        assert_eq!(
            adapter_for_provider_model(&ProviderId::Famous(ProviderKind::Deepseek), "deepseek"),
            AdapterKind::DeepSeek,
        );
    }

    #[test]
    fn openai_provider_keeps_openai_adapter_split() {
        assert_eq!(
            adapter_for_provider_model(&ProviderId::Famous(ProviderKind::Openai), "gpt-4.1"),
            AdapterKind::OpenAI,
        );
        assert_eq!(
            adapter_for_provider_model(&ProviderId::Famous(ProviderKind::Openai), "gpt-5.5"),
            AdapterKind::OpenAIResp,
        );
    }

    /// A custom provider's id reports the four facts its config declares,
    /// and `adapter_for_provider_model` returns the declared adapter
    /// verbatim — the OpenAI per-model refinement applies only to the
    /// famous OpenAI kind, never to a custom OpenAI-compatible endpoint.
    #[test]
    fn custom_provider_id_reports_declared_facts() {
        let id = ProviderId::Custom(Arc::new(CustomProvider {
            label: "local-llama".into(),
            key_env: "LOCAL_LLAMA_KEY".into(),
            endpoint: "https://llama.example/v1/".into(),
            adapter: AdapterKind::OpenAI,
        }));
        assert_eq!(id.label(), "local-llama");
        assert_eq!(id.key_env(), "LOCAL_LLAMA_KEY");
        assert_eq!(id.endpoint(), Some("https://llama.example/v1/"));
        assert_eq!(id.default_adapter(), AdapterKind::OpenAI);
        assert_eq!(id.famous(), None);
        assert_eq!(
            adapter_for_provider_model(&id, "gpt-5.5"),
            AdapterKind::OpenAI,
        );
    }

    #[test]
    fn responses_adapter_routes_system_to_instructions() {
        let req = build_cached_request(
            AdapterKind::OpenAIResp,
            "SYS",
            vec![ChatMessage::user("hi")],
        );
        assert_eq!(req.system.as_deref(), Some("SYS"));
        assert!(req.messages.iter().all(|m| m.role != ChatRole::System));
    }

    #[test]
    fn other_adapters_keep_a_cache_marked_system_message() {
        let req =
            build_cached_request(AdapterKind::Anthropic, "SYS", vec![ChatMessage::user("hi")]);
        assert!(req.system.is_none());
        let first = &req.messages[0];
        assert_eq!(first.role, ChatRole::System);
        assert_eq!(
            first.options.as_ref().and_then(|o| o.cache_control.clone()),
            Some(CacheControl::Ephemeral)
        );
    }

    /// A genuine transport fault on the streaming path arrives as a
    /// `reqwest::Error` boxed inside `WebStream`.  genai exposes no status
    /// for it, so the classifier must fall through to the transport
    /// predicate / keyword heuristic and route it to `Transient` for the
    /// retry loop.  Constructing a `reqwest::Error` directly is not
    /// possible, so this exercises the `Display`-keyword fallback that
    /// covers the `WebStream`-with-string-cause case.
    #[test]
    fn from_genai_classifies_web_stream_error() {
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
        match e {
            ProviderError::Transient {
                cause, attempts, ..
            } => {
                assert!(cause.contains("Web stream error"));
                assert_eq!(attempts, 1, "from_genai stamps the first attempt");
            }
            other => panic!("expected Transient, got {other:?}"),
        }
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

    /// A streaming 429 (`WebStream` boxing `HttpError`) must also classify
    /// as rate-limited — the typed status is recovered by downcasting the
    /// boxed cause.
    #[test]
    fn from_genai_classifies_streaming_429_rate_limit() {
        let e =
            ProviderError::from_genai(&web_stream_http(StatusCode::TOO_MANY_REQUESTS), "gpt-5.5");
        assert!(
            matches!(e, ProviderError::RateLimited { .. }),
            "streaming 429 must classify RateLimited, got {e:?}"
        );
    }

    #[test]
    fn from_genai_classifies_401_api_error() {
        let e = ProviderError::from_genai(
            &web_model_call(StatusCode::UNAUTHORIZED, HeaderMap::new()),
            "gpt-5.5",
        );
        match e {
            ProviderError::Api { status, model, .. } => {
                assert_eq!(status, Some(401));
                assert_eq!(model, "gpt-5.5");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    /// The DeepSeek failure from the session log: a hard 400 arriving on
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

    /// DeepSeek thinking mode rejects a follow-up request unless the prior
    /// assistant turn's `reasoning_content` is echoed back.  The step
    /// projection must therefore attach the captured reasoning as a
    /// `ReasoningContent` part on the assistant message.
    #[test]
    fn step_out_preserves_reasoning_content() {
        let end = StreamEnd {
            captured_content: Some(MessageContent::from("answer")),
            captured_reasoning_content: Some("let me think".into()),
            ..Default::default()
        };
        let out = step_out_from_end("deepseek-v4-pro", end, true);
        assert!(
            out.assistant_message.content.iter().any(|p| matches!(
                p,
                ContentPart::ReasoningContent(r) if r == "let me think"
            )),
            "captured reasoning must round-trip onto the assistant message",
        );
    }

    /// When the provider surfaces no reasoning, no `ReasoningContent` part
    /// is attached — `with_reasoning_content(None)` is a no-op, keeping
    /// non-thinking providers (and DeepSeek's non-thinking models) clean.
    #[test]
    fn step_out_without_reasoning_attaches_nothing() {
        let end = StreamEnd {
            captured_content: Some(MessageContent::from("answer")),
            captured_reasoning_content: None,
            ..Default::default()
        };
        let out = step_out_from_end("deepseek-v4-pro", end, true);
        assert!(
            !out.assistant_message
                .content
                .iter()
                .any(|p| matches!(p, ContentPart::ReasoningContent(_))),
            "no reasoning part when the provider surfaced none",
        );
    }

    /// A variant with no structured status that matches none of the
    /// transient keywords must surface raw as `Other`.
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
    /// OpenRouter `{"error":{"code":NNN}}` JSON shape — the status is read
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

    /// A streaming 5xx (`WebStream` boxing `HttpError`) is likewise
    /// retryable.
    #[test]
    fn from_genai_classifies_streaming_5xx_as_transient() {
        let e = ProviderError::from_genai(
            &web_stream_http(StatusCode::BAD_GATEWAY),
            "anthropic/claude-opus-4",
        );
        assert!(matches!(e, ProviderError::Transient { .. }), "got {e:?}");
    }

    /// A mid-stream `ChatResponse` frame carrying a 500 also routes to
    /// `Transient` via the structural JSON-code read.
    #[test]
    fn from_genai_classifies_json_body_5xx_as_transient() {
        let e = ProviderError::from_genai(
            &genai::Error::ChatResponse {
                model_iden: iden("m"),
                body: serde_json::json!({"error": {"code": 500, "message": "oops"}}),
            },
            "m",
        );
        assert!(matches!(e, ProviderError::Transient { .. }), "got {e:?}");
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

    /// X9: `Usage::parts` is the single content/layout source the plain
    /// `Display` and the TUI's styled renderer both consume.  Pin the
    /// pieces so a future edit to one renderer cannot silently diverge the
    /// other.
    #[test]
    fn usage_parts_are_the_shared_render_source() {
        let u = Usage {
            input: 4_017_400,
            output: 12_800,
            cache_creation: Some(200_000),
            cache_read: Some(3_817_000),
            dollars: 1.2345,
            unmetered: false,
        };
        let p = u.parts();
        assert_eq!(p.input, "4017.4k");
        assert_eq!(p.output, "12.8k");
        assert_eq!(p.cache, Some(("200.0k".into(), "3817.0k".into())));
        assert_eq!(p.cost, "$1.2345");
        // The same pieces must reconstruct the `Display` string verbatim.
        assert_eq!(
            u.to_string(),
            "total 4017.4k in / 12.8k out [200.0k wr/3817.0k rd] · $1.2345"
        );
    }

    /// An unmetered turn (a ChatGPT plan login) reports its cost slot as
    /// "subscription" rather than a dollar figure or `—`.  The token
    /// counts still render, so the line stays informative; the price just
    /// makes no sense for a flat subscription.  Both `parts` and the
    /// `Display` it feeds must agree.
    #[test]
    fn usage_parts_unmetered_reads_subscription() {
        let u = Usage {
            input: 1_000,
            output: 50,
            cache_creation: None,
            cache_read: None,
            dollars: 0.0,
            unmetered: true,
        };
        assert_eq!(u.parts().cost, "subscription");
        assert_eq!(u.to_string(), "total 1000 in / 50 out · subscription");
    }

    #[test]
    fn stamp_attempts_records_count_on_transient() {
        let e = ProviderError::Transient {
            cause: "boom".into(),
            attempts: 1,
            body: None,
        };
        let stamped = stamp_attempts(e, 3);
        match stamped {
            ProviderError::Transient { attempts, .. } => assert_eq!(attempts, 3),
            _ => panic!("expected Transient"),
        }
    }

    /// Rate limits get a strictly larger retry budget and a higher
    /// backoff ceiling than transient failures — the provider loop is
    /// now the only tier that retries 429s, so it has to be the patient
    /// one.
    #[test]
    fn rate_limit_gets_larger_budget_than_transient() {
        let (rl_attempts, rl_ceiling) = retry_limits(&ProviderError::RateLimited {
            retry_after: None,
            cause: "429".into(),
            body: None,
        });
        let (tr_attempts, tr_ceiling) = retry_limits(&ProviderError::Transient {
            cause: "boom".into(),
            attempts: 1,
            body: None,
        });
        assert_eq!(rl_attempts, RATE_LIMIT_MAX_ATTEMPTS);
        assert_eq!(rl_ceiling, RATE_LIMIT_MAX_DELAY_MS);
        assert_eq!(tr_attempts, MAX_ATTEMPTS);
        assert_eq!(tr_ceiling, MAX_DELAY_MS);
        assert!(rl_attempts > tr_attempts);
        assert!(rl_ceiling > tr_ceiling);
    }

    /// Renders the OpenAI-family case: the provider returned a cached
    /// read count but no creation count.  The `wr` half must show `—`
    /// (the field was not reported) rather than `0` (a measured zero).
    #[test]
    fn usage_display_unreported_creation_renders_em_dash() {
        let u = Usage {
            input: 4_017_400,
            output: 12_800,
            cache_creation: None,
            cache_read: Some(3_817_000),
            dollars: 0.0,
            unmetered: false,
        };
        assert_eq!(
            u.to_string(),
            "total 4017.4k in / 12.8k out [— wr/3817.0k rd] · —",
        );
    }

    /// Renders the Anthropic case: both numbers are real measurements,
    /// dollars are known.  This is the existing rendering shape and
    /// must be preserved unchanged.
    #[test]
    fn usage_display_both_fields_reported() {
        let u = Usage {
            input: 4_017_400,
            output: 12_800,
            cache_creation: Some(200_000),
            cache_read: Some(3_817_000),
            dollars: 1.2345,
            unmetered: false,
        };
        assert_eq!(
            u.to_string(),
            "total 4017.4k in / 12.8k out [200.0k wr/3817.0k rd] · $1.2345",
        );
    }

    /// `Usage::default()` leaves both cache fields as `None`; the cache
    /// suffix must be omitted entirely — there's nothing to say.
    #[test]
    fn usage_display_default_omits_cache_suffix() {
        assert_eq!(Usage::default().to_string(), "total 0 in / 0 out · —");
    }

    /// `Some(0)` on both sides is a real measurement (an Anthropic turn
    /// with no cache activity).  The suffix is still omitted because
    /// there is no useful signal — but this differs from the `None`
    /// case only in intent, not in rendering.
    #[test]
    fn usage_display_explicit_zeros_omit_cache_suffix() {
        let u = Usage {
            input: 100,
            output: 50,
            cache_creation: Some(0),
            cache_read: Some(0),
            dollars: 0.0,
            unmetered: false,
        };
        assert_eq!(u.to_string(), "total 100 in / 50 out · —");
    }

    /// AddAssign keeps the `Some` flavour once any turn has populated
    /// it.  Critical: a later `None` turn must not collapse the
    /// accumulator back to `None`, or the session total would lose the
    /// "we know about this" signal.
    #[test]
    fn usage_add_assign_propagates_some() {
        let mut acc = Usage::default();
        acc += Usage {
            input: 1_000,
            output: 50,
            cache_creation: None,
            cache_read: Some(800),
            dollars: 0.0,
            unmetered: false,
        };
        assert_eq!(acc.cache_creation, None);
        assert_eq!(acc.cache_read, Some(800));

        acc += Usage {
            input: 1_500,
            output: 60,
            cache_creation: None,
            cache_read: Some(1_200),
            dollars: 0.0,
            unmetered: false,
        };
        assert_eq!(acc.input, 2_500);
        assert_eq!(acc.cache_creation, None, "None + None stays None");
        assert_eq!(acc.cache_read, Some(2_000), "Some + Some sums");

        acc += Usage {
            input: 500,
            output: 10,
            cache_creation: Some(42),
            cache_read: None,
            dollars: 0.0,
            unmetered: false,
        };
        assert_eq!(acc.cache_creation, Some(42), "None + Some promotes to Some",);
        assert_eq!(
            acc.cache_read,
            Some(2_000),
            "Some + None preserves the running total",
        );
    }

    /// A stalled stream surfaces from `complete`'s select as a
    /// `Transient` error *before* any token streamed — modelled here as
    /// `Attempt::Failed(Transient)`.  Driven through the real retry loop
    /// it must be re-issued up to `MAX_ATTEMPTS` and then surface `Err`
    /// (bounded, never hanging) so the turn fails cleanly instead of
    /// blocking until the harness wall.
    #[test]
    fn idle_timeout_before_token_is_retried_then_surfaced() {
        let calls = std::cell::Cell::new(0u32);
        let out: Result<(), ProviderError> =
            make_runtime().block_on(retry_with_backoff("test", async |_attempt| {
                calls.set(calls.get() + 1);
                Attempt::Failed(ProviderError::Transient {
                    cause: "stream idle: no event within timeout".into(),
                    attempts: 1,
                    body: None,
                })
            }));
        assert_eq!(calls.get(), MAX_ATTEMPTS, "exhausts the transient budget");
        match out {
            Err(ProviderError::Transient { attempts, .. }) => {
                assert_eq!(attempts, MAX_ATTEMPTS, "surfaced with the attempt count")
            }
            other => panic!("expected Transient after the budget, got {other:?}"),
        }
    }

    /// Once tokens have streamed, `complete` reports the idle timeout as
    /// `Attempt::Committed(Transient)`: a re-issue would double-render, so
    /// the retry loop surfaces it immediately on the first attempt.
    #[test]
    fn idle_timeout_after_token_surfaces_without_retry() {
        let calls = std::cell::Cell::new(0u32);
        let out: Result<(), ProviderError> =
            make_runtime().block_on(retry_with_backoff("test", async |_attempt| {
                calls.set(calls.get() + 1);
                Attempt::Committed(ProviderError::Transient {
                    cause: "stream idle: no event within timeout".into(),
                    attempts: 1,
                    body: None,
                })
            }));
        assert_eq!(calls.get(), 1, "committed output is not re-issued");
        assert!(
            matches!(out, Err(ProviderError::Transient { .. })),
            "got {out:?}"
        );
    }

    /// The idle timeout is an idle (between-events) bound, not a total
    /// cap, and even the worst case — the full transient retry budget,
    /// each attempt idling for the whole timeout — stays well under the
    /// 900s terminal-bench harness wall.  This guards against a future
    /// bump of `STREAM_IDLE_TIMEOUT` (or `MAX_ATTEMPTS`) that would
    /// reintroduce the harness-kill hang.
    #[test]
    fn idle_timeout_budget_stays_under_harness_wall() {
        let worst_case = STREAM_IDLE_TIMEOUT * MAX_ATTEMPTS;
        assert!(
            worst_case < Duration::from_secs(900),
            "idle budget {worst_case:?} must stay under the 900s harness wall"
        );
    }
}
