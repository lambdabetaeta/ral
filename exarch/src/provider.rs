//! LLM transport — thin wrapper over genai.
//!
//! `Provider::complete` takes already-rendered [`ChatMessage`]s and
//! streams one assistant reply.  The transcript owns history; the
//! provider only sends bytes and parses responses.

use crate::cancel;
use crate::credential::Credential;
use crate::oauth;
use crate::tls::STREAM_IDLE_TIMEOUT;
use clap::ValueEnum;
use futures_util::StreamExt;
use genai::Headers;
use genai::adapter::AdapterKind;
use genai::chat::{
    CacheControl, ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, MessageOptions,
    StreamEnd, Tool,
};
pub use genai::chat::{ReasoningEffort, StopReason, ToolCall};
use genai::resolver::{AuthData, AuthResolver, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use std::fmt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use serde::{Deserialize, Serialize};

// Per-model max-tokens defaults live inside genai's own adapter table
// (e.g. `AnthropicAdapter::resolve_max_tokens` picks 32k for Opus 4.x,
// 64k for Sonnet/Haiku/4-5).  We previously pinned a hard 4k constant
// here which silently truncated long Opus turns; the fix is to *not
// override* genai's per-model resolution unless the user explicitly
// passes `--max-tokens`.  Provider::max_tokens_override carries the
// `Some` only when the user opted in.

/// Best-effort capability lookup for `model`.  Consults the OR
/// catalog regardless of provider — OR republishes upstream cards for
/// every vendor it routes, so a native Anthropic / OpenAI launch hits
/// the same context-window / tokenizer / canonical-slug data as the
/// OR-fronted equivalent.  (DeepSeek is the pricing exception, not a
/// capabilities exception — capabilities still come from OR.)
/// Missing entries default the record so the banner renders `—` for
/// unknown fields.
pub fn caps_for(model: &str) -> crate::pricing::ModelCaps {
    crate::pricing::caps(model).unwrap_or_default()
}

/// Retry budget for transient stream/network failures.  Three attempts
/// means up to two retries before surfacing the error.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
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

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProviderKind {
    Anthropic,
    Openai,
    Openrouter,
    Deepseek,
    OpencodeZen,
    OpencodeGo,
    Xai,
    Qwen,
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
            // opencode issues one key per account and tells Zen from Go apart
            // by endpoint, so both read the same `OPENCODE_API_KEY`.
            Self::OpencodeZen => ("opencode-zen", "glm-5.1", "OPENCODE_API_KEY"),
            Self::OpencodeGo => ("opencode-go", "glm-5.2", "OPENCODE_API_KEY"),
            Self::Xai => ("xai", "grok-4.3", "XAI_API_KEY"),
            // DashScope's float alias for the current Qwen-Plus, reached over
            // the international OpenAI-compatible endpoint (see `endpoint`).
            Self::Qwen => ("qwen", "qwen3.6-plus", "DASHSCOPE_API_KEY"),
        }
    }

    /// Base URL for providers that need a custom endpoint.
    /// Must end with `/` — genai uses `Url::join` to append the
    /// service path (e.g. `chat/completions`).
    pub fn endpoint(self) -> Option<&'static str> {
        match self {
            Self::Openrouter => Some("https://openrouter.ai/api/v1/"),
            Self::OpencodeZen => Some("https://opencode.ai/zen/v1/"),
            Self::OpencodeGo => Some("https://opencode.ai/zen/go/v1/"),
            Self::Qwen => Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1/"),
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
            Self::OpencodeZen | Self::OpencodeGo => AdapterKind::OpenAI,
            Self::Xai => AdapterKind::Xai,
            Self::Qwen => AdapterKind::OpenAI,
        }
    }

    /// Whether this provider bills as a flat subscription rather than per
    /// token. opencode Go is a flat $10/mo plan over an OpenAI-compatible
    /// gateway; opencode Zen on the same gateway is pay-as-you-go. A ChatGPT
    /// plan login is a flat subscription too, but it rides the OAuth
    /// credential rather than this `ProviderKind` property.
    pub fn flat_rate(self) -> bool {
        matches!(self, Self::OpencodeGo)
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

/// A signed-in ChatGPT account, as a selectable provider identity.
///
/// A ChatGPT plan authorises over OAuth rather than an API key, and several
/// accounts can be signed in at once. Each is its own [`ProviderId`] so the
/// `/model` picker, the persisted selection, and the credential store carry it
/// exactly like any other provider — switching accounts *is* switching the
/// selected identity, with no second selection dimension threaded anywhere.
///
/// It holds only what selection needs: the [`label`](ProviderId::label) the
/// account is keyed and displayed by, and the OpenAI-side account id it maps
/// to. The live tokens live in the [`crate::credential::Credential::OAuth`]
/// cell the store binds under this id, not here; the `chatgpt-account-id` a
/// request carries is read from that bound token, not from this struct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatGptAccount {
    /// The OpenAI account id this selection maps to.
    pub account_id: String,
    /// The account's stable, human-readable handle — its login email, or the
    /// account id when none was issued. Its unique key in the store/picker.
    pub label: String,
}

impl ChatGptAccount {
    /// The selectable identity for a stored login.
    pub fn from_token(token: &oauth::OAuthToken) -> Self {
        Self {
            account_id: token.account_id.clone(),
            label: token.label(),
        }
    }
}

/// A provider identity: a famous [`ProviderKind`], a custom-declared one, or a
/// signed-in ChatGPT account ([`ChatGptAccount`]).
///
/// This is the single abstraction credential resolution, model listing, and
/// transport building consume, so a custom provider — or a ChatGPT account —
/// flows through exactly the same machinery as a famous one. Each downstream
/// call site reads the facts it needs through [`Self::label`] /
/// [`Self::key_env`] / [`Self::endpoint`] / [`Self::default_adapter`] and
/// rarely matches on the variant. The `Custom` and `ChatGpt` arms hold an
/// [`Arc`] so the id stays cheap to clone and to use as a map key.
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
    /// A signed-in ChatGPT account, authorising over OAuth.
    ChatGpt(Arc<ChatGptAccount>),
}

impl ProviderId {
    /// The stable label — `ProviderKind::info().0` for a famous provider, the
    /// config map key for a custom one, the account handle for a ChatGPT login.
    pub fn label(&self) -> &str {
        match self {
            Self::Famous(kind) => kind.info().0,
            Self::Custom(c) => &c.label,
            Self::ChatGpt(a) => &a.label,
        }
    }

    /// The environment variable this provider's API key is read from, or
    /// `None` for a provider that does not authenticate off an env key — a
    /// ChatGPT account authorises over OAuth, so it is loaded from the token
    /// store rather than swept from the environment.
    pub fn key_env(&self) -> Option<&str> {
        match self {
            Self::Famous(kind) => Some(kind.info().2),
            Self::Custom(c) => Some(&c.key_env),
            Self::ChatGpt(_) => None,
        }
    }

    /// The base URL for a provider that needs a custom endpoint, or `None`
    /// when the native adapter's default target is used. A custom provider
    /// always has one; a ChatGPT account redirects to the Codex backend
    /// through its OAuth client, not through this endpoint.
    pub fn endpoint(&self) -> Option<&str> {
        match self {
            Self::Famous(kind) => kind.endpoint(),
            Self::Custom(c) => Some(&c.endpoint),
            Self::ChatGpt(_) => None,
        }
    }

    /// The genai adapter this provider speaks by default. For a famous
    /// provider this is the per-kind default; a custom provider names its
    /// wire protocol in the config and carries the resolved adapter; a ChatGPT
    /// account speaks the OpenAI Responses adapter to the Codex backend.
    pub fn default_adapter(&self) -> AdapterKind {
        match self {
            Self::Famous(kind) => kind.default_adapter(),
            Self::Custom(c) => c.adapter,
            Self::ChatGpt(_) => AdapterKind::OpenAIResp,
        }
    }

    /// Whether this provider bills as a flat subscription rather than per
    /// token. A famous provider delegates to [`ProviderKind::flat_rate`]; a
    /// custom provider is never flat-rate yet — the `config.ral` schema does
    /// not declare it, so this is a future slice's extension point. A ChatGPT
    /// account is unmetered too, but rides its OAuth credential (the bound
    /// `token_cell`) rather than this property — so it is not `flat_rate`.
    pub fn flat_rate(&self) -> bool {
        match self {
            Self::Famous(kind) => kind.flat_rate(),
            Self::Custom(_) | Self::ChatGpt(_) => false,
        }
    }

    /// The famous kind, when this is a built-in provider — the few sites that
    /// genuinely need the enum (the OpenAI per-model adapter refinement, the
    /// persisted-selection round-trip) ask for it; everything else reads the
    /// facts above.
    pub fn famous(&self) -> Option<ProviderKind> {
        match self {
            Self::Famous(kind) => Some(*kind),
            Self::Custom(_) | Self::ChatGpt(_) => None,
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

/// Humanise a token count — the one rule every token readout shares: the
/// usage line, the startup banner's context / limit fields, and the
/// per-agent token tallies.  Under 10k prints in full (`0`, `9123`); from
/// 10k to <1M shows one decimal with a bare `.0` dropped (`46.6k`, but
/// `200k` not `200.0k`); 1M and over keeps one decimal (`1.0m`, `1.5m`).
/// One source of truth so the plain [`Display`] and the TUI's styled
/// renderers cannot drift (X9).
pub fn humanize_tokens(n: u64) -> String {
    // Tier on the rounded display, not the raw count: a value within ~50
    // tokens of 1M rounds to `1.0m`, never `1000k`.
    if n >= 999_950 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        let k = format!("{:.1}", n as f64 / 1_000.0);
        format!("{}k", k.strip_suffix(".0").unwrap_or(&k))
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
    /// The model's reasoning trace for this step, captured at the `End`
    /// frame.  Already attached to `assistant_message` for round-tripping;
    /// surfaced separately here so the frontend can render it as the
    /// answer's shadow without re-parsing the message's content parts.
    pub reasoning: Option<String>,
    pub usage: Usage,
    /// Provider-normalised stop reason for this turn.  `None` means the
    /// adapter did not surface one (most non-Anthropic adapters do), or
    /// the stream broke before one arrived (see [`CutShort::Stalled`]).
    pub stop_reason: Option<StopReason>,
    /// Set when the assistant turn ended short of the model's own stop,
    /// with the partial text already committed: the session re-drives the
    /// turn with a `continue` nudge instead of treating the short reply as
    /// final.  `None` on a turn that reached its own stop reason.
    pub cut_short: Option<CutShort>,
}

/// Why an assistant turn ended before the model chose to stop.  Both
/// causes leave a partial assistant message that the session commits and
/// continues from; they differ only in the operator-facing note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CutShort {
    /// The output-token cap (`max_tokens`) truncated the turn — the model
    /// was mid-text or about to emit a tool call.  The operator can raise
    /// the ceiling with `--max-tokens`.
    OutputCap,
    /// The stream broke mid-response *after* partial text had rendered: an
    /// idle stall, a transport error frame, or a close with no `End` frame.
    /// Re-issuing would double-render the streamed prefix, so it is kept
    /// and the turn continues.  Carries the underlying cause for the note.
    Stalled(String),
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
    /// output cap ([`CutShort::OutputCap`]) or a mid-stream stall
    /// ([`CutShort::Stalled`]).  Raised by [`crate::agent::Agent::apply`]
    /// **after** appending the partial assistant message, so re-prompting
    /// with `continue` preserves the partial work as transcript context.
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
                ProviderError::RateLimited {
                    retry_after,
                    cause: msg,
                    body,
                }
            }
            Fault::Status { status, body, .. } if status.is_server_error() => {
                ProviderError::Transient {
                    cause: msg,
                    attempts: 1,
                    body,
                }
            }
            // Any remaining non-success status (4xx, redirects) is a
            // request the user must change — not retryable.
            Fault::Status { status, body, .. } => ProviderError::Api {
                status: Some(status.as_u16()),
                model: model.to_string(),
                message: msg.clone(),
                url: extract_url(&msg),
                body,
            },
            // A genuine transport fault — connect, timeout, or a body that
            // dropped or failed to decode mid-flight — with no HTTP status
            // ever received.  The canonical retryable failure.
            Fault::Transport => ProviderError::Transient {
                cause: msg,
                attempts: 1,
                body: None,
            },
            // No status, no transport leaf: an input / auth / parse fault
            // the user must act on.  Rendered raw.
            Fault::Terminal => ProviderError::Other(msg),
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
/// differently — OpenAI nests the detail under `error`, Anthropic sends
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
    /// [`CutShort::Stalled`] note wants to quote.  For the transient/other
    /// failures a committed stall produces this is the raw stream cause
    /// (e.g. the `reqwest` read-timeout text genai surfaces as a
    /// `"Web stream error"`); for anything else it falls back to the full
    /// `Display`.
    fn brief(&self) -> String {
        match self {
            ProviderError::Transient { cause, .. } | ProviderError::RateLimited { cause, .. } => {
                cause.clone()
            }
            ProviderError::Other(s) => s.clone(),
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
            ProviderError::Cancelled(where_) => format!("cancelled {where_}"),
            ProviderError::Transient { body, .. } => {
                with_body_message("web stream failed", body.as_ref())
            }
            ProviderError::RateLimited { body, .. } => {
                with_body_message("rate limited", body.as_ref())
            }
            ProviderError::Api {
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
            ProviderError::Truncated { reason } => format!("reply cut off ({reason})"),
            ProviderError::Other(s) => first_line(s).to_string(),
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
            ProviderError::Truncated { reason } => {
                write!(f, "reply cut off before completion (reason={reason})")
            }
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
    /// Success — return it.  A committed mid-stream stall also lands here:
    /// `complete` keeps the streamed prefix as a [`CutShort::Stalled`]
    /// [`StepOut`] rather than retrying (a re-issue would double-render),
    /// so the driver never needs a distinct "don't retry" failure class.
    Done(T),
    /// Failed; the driver retries when the error is transient/rate-limited
    /// and budget remains (see [`retry_limits`]), otherwise surfaces it.
    Failed(ProviderError),
}

/// Drive `one` under the transient/rate-limit retry policy: bounded,
/// per-error exponential backoff (see [`retry_limits`] / [`backoff_sleep`]),
/// cancellation-aware.  `cancel_site` labels where a cancel that lands
/// between attempts is attributed.  `one` is invoked with the 1-based
/// attempt number and performs exactly one request.
async fn retry_with_backoff<T>(
    cancel_site: &'static str,
    cancel: &cancel::Token,
    mut one: impl AsyncFnMut(u32) -> Attempt<T>,
) -> Result<T, ProviderError> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled(cancel_site));
        }
        let err = match one(attempt).await {
            Attempt::Done(v) => return Ok(v),
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
            _ = wait_for_cancel(cancel) => return Err(ProviderError::Cancelled(cancel_site)),
            _ = backoff_sleep(attempt, retry_after, max_delay_ms) => {}
        }
    }
}

/// The per-selection request tuning the `/model` overlay carries alongside the
/// model: the reasoning effort, the sampling temperature, and the
/// nucleus-sampling top-p.  Each is `None` for "auto" — the option is then
/// *not* set on the wire and genai applies the adapter's per-model default,
/// exactly as before tuning existed.  The overlay edits it, [`crate::state`]
/// persists it, and [`Engine::complete`] folds it into the [`ChatOptions`].
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct Tuning {
    /// The reasoning effort, or `None` for the adapter default.
    pub effort: Option<ReasoningEffort>,
    /// The sampling temperature in `0.0..=2.0`, or `None` for the adapter
    /// default.
    pub temperature: Option<f64>,
    /// The nucleus-sampling top-p in `0.0..=1.0`, or `None` for the adapter
    /// default.
    pub top_p: Option<f64>,
}

impl Tuning {
    /// The tuning a fresh selection starts with: reasoning on at `medium`,
    /// temperature left to the adapter. Distinct from [`Tuning::default`]
    /// (all-auto), which remains the honest "let the adapter decide" sentinel
    /// the `auto` rung still selects — this is only the startup fallback so a
    /// new session thinks by default rather than inheriting Anthropic's
    /// thinking-off default.
    pub fn initial() -> Self {
        Self {
            effort: Some(ReasoningEffort::Medium),
            temperature: None,
            top_p: None,
        }
    }
}

/// Effort identity by variant name — genai's [`ReasoningEffort`] is not
/// `PartialEq`, and two efforts are the same iff they name the same rung
/// (the overlay never constructs the value-carrying `Budget`).
impl PartialEq for Tuning {
    fn eq(&self, other: &Self) -> bool {
        let key = |e: &Option<ReasoningEffort>| e.as_ref().map(|e| e.variant_name());
        key(&self.effort) == key(&other.effort)
            && self.temperature == other.temperature
            && self.top_p == other.top_p
    }
}

pub struct Provider {
    backend: Backend,
    id: ProviderId,
    model: String,
    /// User-supplied `--max-tokens` override.  `None` means we don't
    /// set the option at all and genai applies its per-model default
    /// (the correct path for Opus, Sonnet, Haiku and the other adapters
    /// that ship their own resolution tables).
    max_tokens_override: Option<u32>,
    /// The reasoning-effort + temperature tuning for this selection, set by
    /// the `/model` overlay (or restored from persisted state at startup).
    tuning: Tuning,
}

/// The transport behind a [`Provider`].  The live backend borrows the shared
/// [`Engine`]'s runtime and holds the [`Transport`] it resolved for its
/// credential; the scripted backend (test-only) replays canned [`StepOut`] /
/// [`SummaryOut`] outcomes so the session turn driver can be exercised
/// end-to-end without a network.
enum Backend {
    Live {
        engine: Arc<Engine>,
        transport: Arc<Transport>,
    },
    Scripted(scripted::Script),
}

/// Identifies a cached transport by the two things that fix a genai client:
/// the credential (its label) and the wire `adapter`.  An OpenAI key spans
/// both adapters — `gpt-4o` speaks chat completions, `gpt-5` the Responses API
/// — so they must not share one client even though they share a credential;
/// every other provider resolves a single, model-independent adapter and so
/// keeps one entry per credential.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TransportKey {
    cred: String,
    adapter: AdapterKind,
}

impl TransportKey {
    fn for_selection(id: &ProviderId, model: &str) -> Self {
        Self {
            cred: id.label().to_string(),
            adapter: adapter_for_provider_model(id, model),
        }
    }
}

/// What actually sends bytes to an LLM backend.  One per credential, built
/// once and shared by every [`Provider`] on that credential.
pub(crate) struct Transport {
    client: Client,
    /// The wire adapter this transport speaks, resolved once at build time
    /// from the `(id, model)` that fixed the client.  It is an intrinsic
    /// property of the transport — the same value [`TransportKey`] keyed on —
    /// so the request path reads it here rather than recomputing it per turn.
    adapter: AdapterKind,
    /// The shared login cell when this credential authenticates off a ChatGPT
    /// plan; `None` for an API-key credential.  A turn refreshes through it
    /// before a request when the access token is near expiry.
    token_cell: Option<Arc<Mutex<oauth::OAuthToken>>>,
    /// Whether this credential's plan is a flat subscription declared by its
    /// [`ProviderId`] (opencode Go's $10/mo gateway) rather than by an OAuth
    /// login.  Combined with [`Self::token_cell`] it is the second way a
    /// transport's turns can be unmetered — see [`Self::metered`].
    flat_rate: bool,
}

/// One per process — the shared runtime and the per-credential transports
/// warmed beneath all Providers.  Each [`Provider`] holds the
/// [`Arc<Transport>`] it resolved at build time, so the map is touched only on
/// the cold warm path, never per request.
pub struct Engine {
    runtime: tokio::runtime::Runtime,
    /// Stable per-process identifier sent as OpenAI's `prompt_cache_key` on
    /// every request — a routing hint so all calls from one exarch process
    /// land on the same backend shard, raising cache-hit rate.  Other
    /// providers ignore unknown fields, so it is harmless elsewhere.
    cache_key: String,
    transports: Mutex<HashMap<TransportKey, Arc<Transport>>>,
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
    /// Build a live provider selection on a shared [`Engine`].  It warms the
    /// credential's [`Transport`] in the engine (or reuses an already-warmed
    /// one) and holds it alongside the per-agent selection.  Cheap: no
    /// runtime; the client is built once per credential and shared thereafter.
    pub(crate) fn build(
        engine: Arc<Engine>,
        id: &ProviderId,
        model: String,
        cred: &Credential,
        max_tokens_override: Option<u32>,
        tuning: Tuning,
    ) -> Self {
        let transport = engine.transport_for(id, &model, cred);
        Self {
            backend: Backend::Live { engine, transport },
            id: id.clone(),
            model,
            max_tokens_override,
            tuning,
        }
    }

    /// A test provider that replays scripted outcomes instead of
    /// talking to genai.  No tokio runtime, no network: `complete` and
    /// `summarize` pop their next outcome from `script` in order.  This
    /// is the seam the `tests/` harness drives [`crate::agent::Agent::apply`]
    /// through — see [`scripted::Script`].
    pub fn scripted(model: &str, kind: ProviderKind, script: scripted::Script) -> Self {
        Self {
            backend: Backend::Scripted(script),
            id: ProviderId::Famous(kind),
            model: model.to_string(),
            max_tokens_override: None,
            tuning: Tuning::default(),
        }
    }

    /// The `--max-tokens` override, if the user passed one.  `None`
    /// means the banner should render "auto" — genai's adapter picks a
    /// per-model default at request time.
    pub fn max_tokens_override(&self) -> Option<u32> {
        self.max_tokens_override
    }

    /// The reasoning-effort + temperature tuning bound at build time — read
    /// by the `/model` overlay to seed its controls with the live values.
    pub fn tuning(&self) -> &Tuning {
        &self.tuning
    }

    /// The resolved model name, used by the banner and the capabilities
    /// lookup.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The provider identity, used to resolve the adapter dynamically.
    pub fn id(&self) -> &ProviderId {
        &self.id
    }


    /// This model's total context window in tokens, when the catalog
    /// lists it — the denominator the compaction trigger measures real
    /// context pressure against.  A live catalog lookup (a cheap `HashMap`
    /// get), so it self-heals once [`crate::pricing::ensure_loaded`]
    /// completes; `None` for a native provider or before the catalog
    /// loads, where compaction falls back to the byte heuristic.
    pub fn context_window(&self) -> Option<u64> {
        crate::pricing::context_window(&self.model)
    }

    /// How this provider's plan reads — a flat subscription (and which
    /// flavour) or a metered API key. A subscription's turns carry no
    /// per-token price; the flavour drives the status/picker label
    /// ([`oauth::provider_label`]). A ChatGPT plan login is the OAuth
    /// flavour (login cell present), an [`ProviderId::flat_rate`] provider
    /// the generic flavour; a scripted backend is never a subscription.
    pub fn subscription(&self) -> oauth::Subscription {
        match &self.backend {
            Backend::Live { transport, .. } => transport.subscription(),
            Backend::Scripted(_) => oauth::Subscription::Metered,
        }
    }

    /// Stream one assistant response for an already-rendered message list.
    ///
    /// Retries with bounded exponential backoff on `Transient` and
    /// `RateLimited` errors **only when zero tokens have streamed this
    /// attempt** — once any token has flowed to `on_text` the UI has
    /// committed to a partial render, and re-streaming the request
    /// would double tokens.  See the design doc's option (A).
    /// Run one provider round-trip, streaming assistant text through
    /// `on_text`.  `cancel` is the *request-local* cancellation handle: the
    /// foreground turn passes its root token (linked to the signal slot, so
    /// Esc cancels it), and an async agent passes its registry token (so
    /// `agent_cancel` / `/clear` / the worker ceiling cancel it without
    /// touching the foreground request).  Two concurrent requests no longer
    /// share the one process-global slot.
    pub(crate) fn complete<F: FnMut(&str)>(
        &self,
        system: &str,
        messages: Vec<ChatMessage>,
        tools: &[&'static dyn crate::tools::Tool],
        on_text: &mut F,
        cancel: &cancel::Token,
    ) -> Result<StepOut, ProviderError> {
        match &self.backend {
            Backend::Live { engine, transport } => {
                engine.complete(
                    transport,
                    &self.model,
                    self.max_tokens_override,
                    &self.tuning,
                    system,
                    messages,
                    tools,
                    on_text,
                    cancel,
                )
            }
            Backend::Scripted(s) => s.complete(&self.model, on_text),
        }
    }

    /// Summarise already-rendered transcript messages.
    ///
    /// Retries unconditionally on Transient/RateLimited — there is no
    /// streamed side effect to worry about.  `cancel` is the request-local
    /// handle, as for [`Provider::complete`].
    pub fn summarize(
        &self,
        system: &str,
        messages: Vec<ChatMessage>,
        max_tokens: u32,
        cancel: &cancel::Token,
    ) -> Result<SummaryOut, ProviderError> {
        match &self.backend {
            Backend::Live { engine, transport } => {
                engine.summarize(transport, &self.model, system, messages, max_tokens, cancel)
            }
            Backend::Scripted(s) => s.summarize(&self.model),
        }
    }
}

impl Transport {
    /// Build the genai client + auth for `cred`, bound to `id`+`model`.  The
    /// model only seeds the adapter pin (OpenAI chat vs. responses); every
    /// other provider's adapter is model-independent.
    fn build(id: &ProviderId, model: &str, cred: &Credential) -> Arc<Self> {
        let token_cell = match cred {
            Credential::OAuth(cell) => Some(cell.clone()),
            Credential::ApiKey(_) => None,
        };
        let (client, adapter) = build_client(id, model, cred);
        Arc::new(Self {
            client,
            adapter,
            token_cell,
            flat_rate: id.flat_rate(),
        })
    }

    /// Whether this transport's turns are billed per token. An API-key
    /// credential is metered; a flat subscription — a ChatGPT plan login
    /// ([`Self::token_cell`]) or an [`ProviderId::flat_rate`] plan
    /// ([`Self::flat_rate`]) — is unmetered.
    fn metered(&self) -> bool {
        self.token_cell.is_none() && !self.flat_rate
    }

    /// How this transport's plan reads — see [`Provider::subscription`].
    fn subscription(&self) -> oauth::Subscription {
        if self.token_cell.is_some() {
            oauth::Subscription::ChatGpt
        } else if self.flat_rate {
            oauth::Subscription::FlatRate
        } else {
            oauth::Subscription::Metered
        }
    }
}

impl Engine {
    /// Build the shared runtime and prime pricing once.  Transports are warmed
    /// lazily per credential by [`Self::transport_for`].
    pub(crate) fn new() -> Arc<Self> {
        let runtime = make_runtime();
        prime_pricing(&runtime);
        Arc::new(Self {
            runtime,
            cache_key: fresh_cache_key(),
            transports: Mutex::new(HashMap::new()),
        })
    }

    /// The transport for `cred` bound to `id`+`model`, building and caching it
    /// on first use.  One client per credential and wire adapter, deduped by
    /// [`TransportKey`], so a `/model` re-selection onto an already-warmed
    /// (credential, adapter) — or a new agent on it — reuses the client.
    /// Touched only on this cold warm path; the hot path holds the returned
    /// `Arc` directly.
    fn transport_for(&self, id: &ProviderId, model: &str, cred: &Credential) -> Arc<Transport> {
        self.transports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(TransportKey::for_selection(id, model))
            .or_insert_with(|| Transport::build(id, model, cred))
            .clone()
    }

    /// Renew the ChatGPT access token when it is near expiry, so the request
    /// that follows authenticates with a live token.  A no-op for an API-key
    /// transport.  A refresh failure is logged and left to surface as the
    /// request's own auth error rather than aborting the turn here.
    fn refresh_if_stale(&self, transport: &Transport) {
        let Some(cell) = &transport.token_cell else {
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
                // Upsert this account's token, leaving any other signed-in
                // account untouched — a whole-file overwrite would drop them.
                let _ = oauth::save_one(&fresh);
                *cell.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
            }
            Err(e) => eprintln!("exarch: ChatGPT token refresh failed: {e}"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn complete<F: FnMut(&str)>(
        &self,
        transport: &Transport,
        model: &str,
        max_tokens_override: Option<u32>,
        tuning: &Tuning,
        system: &str,
        messages: Vec<ChatMessage>,
        tools: &[&'static dyn crate::tools::Tool],
        on_text: &mut F,
        cancel: &cancel::Token,
    ) -> Result<StepOut, ProviderError> {
        self.refresh_if_stale(transport);
        let adapter = transport.adapter;
        let req_template = build_cached_request(adapter, system, messages);
        // Resolved once: the tool set is identical across retry attempts, so
        // the request closure clones this rather than rebuilding it per try.
        let tool_defs = tool_defs(tools);
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
        // The `/model` overlay's tuning, set only when the user moved a knob
        // off "auto"; an unset knob leaves the adapter's default in force.
        if let Some(effort) = &tuning.effort {
            options = options.with_reasoning_effort(effort.clone());
        }
        if let Some(temperature) = tuning.temperature {
            options = options.with_temperature(temperature);
        }
        if let Some(top_p) = tuning.top_p {
            options = options.with_top_p(top_p);
        }

        let step = self.runtime.block_on(retry_with_backoff(
            "before request",
            cancel,
            async |_attempt| {
                let mut req = req_template.clone();
                req.tools = Some(tool_defs.clone());
                let mut seen_any_token = false;
                // Mirrors what `on_text` rendered, so a committed stall can
                // commit exactly the prefix the user already saw.
                let mut streamed = String::new();
                let attempt_result: Result<StreamEnd, ProviderError> = async {
                    let mut resp = tokio::select! {
                        biased;
                        _ = wait_for_cancel(cancel) => {
                            return Err(ProviderError::Cancelled("before request"));
                        }
                        // A fresh `sleep` per select entry bounds the connect
                        // phase — which `tls::client`'s `read_timeout` does
                        // not cover — and time-to-first-event; surfaced as a
                        // transient transport error so the retry budget
                        // re-issues it (no token has streamed yet).
                        _ = tokio::time::sleep(STREAM_IDLE_TIMEOUT) => {
                            return Err(ProviderError::Transient {
                                cause: "stream idle: no response within timeout".into(),
                                attempts: 1,
                                body: None,
                            });
                        }
                        r = transport.client.exec_chat_stream(model, req, Some(&options)) => {
                            r.map_err(|e| ProviderError::from_genai(&e, model))?
                        }
                    };
                    loop {
                        // No idle timer here: a `ping`, an SSE keepalive, or a
                        // partial frame that genai consumes without yielding a
                        // decoded `ChatStreamEvent` would falsely read as idle
                        // at this level even while bytes flow.  Liveness is
                        // measured at the byte/SSE level by `tls::client`'s
                        // `read_timeout`; a genuinely stalled socket trips it
                        // and surfaces here as `Some(Err(_))` carrying a
                        // `reqwest` timeout, which `from_genai` classifies as
                        // transient via its [`Fault::Transport`] leaf.
                        let event = tokio::select! {
                            biased;
                            _ = wait_for_cancel(cancel) => {
                                return Err(ProviderError::Cancelled("mid-stream"));
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
                                seen_any_token |= !c.content.is_empty();
                                streamed.push_str(&c.content);
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
                    Err(ProviderError::Transient {
                        cause: "stream ended without End event".into(),
                        attempts: 1,
                        body: None,
                    })
                }
                .await;

                match attempt_result {
                    Ok(end) => Attempt::Done(step_out_from_end(model, end, transport.metered(), adapter)),
                    // A cancel is surfaced as-is regardless of streamed tokens.
                    Err(e @ ProviderError::Cancelled(_)) => Attempt::Failed(e),
                    // Visible text already streamed, then the stream broke: a
                    // re-issue would double-render, so commit the streamed
                    // prefix as a cut-short turn and let the session continue
                    // it (mirrors the output-cap truncation path) rather than
                    // discarding the work and ending the run.
                    Err(e) if seen_any_token => {
                        Attempt::Done(stalled_step_out(model, &streamed, &e, transport.metered(), adapter))
                    }
                    Err(e) => Attempt::Failed(e),
                }
            },
        ))?;

        Ok(step)
    }

    #[allow(clippy::too_many_arguments)]
    fn summarize(
        &self,
        transport: &Transport,
        model: &str,
        system: &str,
        mut messages: Vec<ChatMessage>,
        max_tokens: u32,
        cancel: &cancel::Token,
    ) -> Result<SummaryOut, ProviderError> {
        self.refresh_if_stale(transport);
        let adapter = transport.adapter;
        messages.push(ChatMessage::user(
            "Summarise the conversation so far concisely: \
            the user's task, what has been tried, what worked, what state the \
            shell is in (cwd, env, defined names), and any open subtasks. \
            A prior summary may appear first — fold it in. \
            Return only the summary, no preamble.",
        ));
        let req_template = build_cached_request(adapter, system, messages);
        let options = ChatOptions::default()
            .with_max_tokens(max_tokens)
            .with_prompt_cache_key(&self.cache_key);

        let resp = self.runtime.block_on(retry_with_backoff(
            "during summary",
            cancel,
            async |_attempt| {
                let req = req_template.clone();
                let r = tokio::select! {
                    biased;
                    _ = wait_for_cancel(cancel) => {
                        return Attempt::Failed(ProviderError::Cancelled("during summary"));
                    }
                    // `summarize` is non-streaming, so there are no
                    // incremental events to idle between: the same budget
                    // bounds the whole `exec_chat` request.  `tls::client`'s
                    // `read_timeout` already covers a stall once bytes are
                    // flowing, but not the connect phase that precedes it;
                    // this select bounds connect too and yields an explicit
                    // typed `Transient` rather than leaning on the `Display`
                    // heuristic that a non-streaming `reqwest` timeout would
                    // otherwise fall through to.  No partial output is ever
                    // rendered, so it is `Failed` (retryable), never
                    // `Committed`.
                    _ = tokio::time::sleep(STREAM_IDLE_TIMEOUT) => {
                        return Attempt::Failed(ProviderError::Transient {
                            cause: "summary request: no response within timeout".into(),
                            attempts: 1,
                            body: None,
                        });
                    }
                    r = transport.client.exec_chat(model, req, Some(&options)) => r,
                };
                match r {
                    Ok(resp) => Attempt::Done(resp),
                    Err(e) => Attempt::Failed(ProviderError::from_genai(&e, model)),
                }
            },
        ))?;

        // A summary that itself hit the `max_tokens` cap is incomplete;
        // committing it would silently drop everything past the cut-off.
        // Surface it as `Truncated` so `Agent::compact` keeps the
        // un-summarised history rather than compacting to a half summary.
        if matches!(resp.stop_reason, Some(StopReason::MaxTokens(_))) {
            return Err(ProviderError::Truncated {
                reason: "summary exceeded the compaction token budget".into(),
            });
        }
        let text = resp.first_text().unwrap_or("").to_string();
        Ok(SummaryOut {
            summary: text,
            usage: usage_from(model, &resp.usage, transport.metered(), adapter),
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
fn step_out_from_end(model: &str, end: StreamEnd, metered: bool, adapter: AdapterKind) -> StepOut {
    let raw_usage = end.captured_usage.clone().unwrap_or_default();
    let stop_reason = end.captured_stop_reason.clone();
    let reasoning = end.captured_reasoning_content.clone();
    let content = end.captured_content.clone().unwrap_or_default();
    let tool_calls = end.captured_into_tool_calls().unwrap_or_default();
    let cut_short = stop_reason
        .as_ref()
        .filter(|r| matches!(r, StopReason::MaxTokens(_)))
        .map(|_| CutShort::OutputCap);
    StepOut {
        assistant_message: ChatMessage::assistant(content)
            .with_reasoning_content(reasoning.clone()),
        tool_calls,
        reasoning,
        usage: usage_from(model, &raw_usage, metered, adapter),
        stop_reason,
        cut_short,
    }
}

/// Project a committed mid-stream interruption into a [`StepOut`].  Some
/// visible text streamed (and was already rendered) before the stream
/// stalled, errored, or closed without an `End` frame, so re-issuing would
/// double-render: the streamed prefix becomes a text-only assistant message
/// marked [`CutShort::Stalled`], which the session commits and continues
/// from.  No `End` frame arrived, so there is no usage to meter and no tool
/// call to capture.
fn stalled_step_out(model: &str, streamed: &str, cause: &ProviderError, metered: bool, adapter: AdapterKind) -> StepOut {
    StepOut {
        assistant_message: ChatMessage::assistant(streamed.to_string()),
        tool_calls: Vec::new(),
        reasoning: None,
        usage: usage_from(model, &genai::chat::Usage::default(), metered, adapter),
        stop_reason: None,
        cut_short: Some(CutShort::Stalled(cause.brief())),
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

fn tool_defs(tools: &[&'static dyn crate::tools::Tool]) -> Vec<Tool> {
    tools
        .iter()
        .map(|t| {
            Tool::new(t.name())
                .with_description(t.desc())
                .with_schema(t.schema().clone())
        })
        .collect()
}

async fn wait_for_cancel(cancel: &cancel::Token) {
    while !cancel.is_cancelled() {
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
/// model's price card for `adapter`.  Used by both [`Provider::complete`] and
fn usage_from(model: &str, raw: &genai::chat::Usage, metered: bool, adapter: AdapterKind) -> Usage {
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
        crate::pricing::lookup_for(model, adapter)
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
/// [`crate::agent::Agent::apply`] end-to-end with no network.  A
/// [`Script`] is a queue of canned `complete` replies (each optionally
/// streaming some text first) and a queue of `summarize` replies,
/// consumed in order.  Running past the end of either queue panics — a
/// test that under-scripts its turn should fail loudly, not hang on a
/// real request.
pub mod scripted {
    use super::{CutShort, ProviderError, StepOut, SummaryOut, Usage};
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
                    reasoning: None,
                    usage: Usage::default(),
                    stop_reason: Some(StopReason::Completed("end_turn".into())),
                    cut_short: None,
                }),
            }
        }

        /// A turn whose stream stalled after `text` had rendered: the
        /// model-facing dual of a live committed mid-stream stall.  Streams
        /// `text` to `on_text` and projects a [`CutShort::Stalled`] step so
        /// the session commits the partial reply and continues the turn.
        pub fn stalled(text: &str) -> Self {
            Self {
                text: text.to_string(),
                outcome: Ok(StepOut {
                    assistant_message: ChatMessage::assistant(text.to_string()),
                    tool_calls: Vec::new(),
                    reasoning: None,
                    usage: Usage::default(),
                    stop_reason: None,
                    cut_short: Some(CutShort::Stalled(
                        "Web stream error: operation timed out".into(),
                    )),
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
                    reasoning: None,
                    usage: Usage::default(),
                    stop_reason: Some(StopReason::ToolCall("tool_use".into())),
                    cut_short: None,
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
                    reasoning: None,
                    usage: Usage::default(),
                    stop_reason: Some(StopReason::MaxTokens("max_tokens".into())),
                    cut_short: Some(CutShort::OutputCap),
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
                    reasoning: None,
                    usage: Usage::default(),
                    stop_reason: Some(StopReason::Completed("end_turn".into())),
                    cut_short: None,
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
    }

    /// A scripted run: `completes` are popped one per [`Provider::complete`]
    /// call.  Build with [`Script::new`] and chain [`Script::then`].
    ///
    /// [`Provider::complete`]: super::Provider::complete
    pub struct Script {
        completes: Mutex<VecDeque<Reply>>,
    }

    impl Script {
        pub fn new() -> Self {
            Self {
                completes: Mutex::new(VecDeque::new()),
            }
        }

        /// Append a `complete` reply.
        pub fn then(self, reply: Reply) -> Self {
            self.completes.lock().unwrap().push_back(reply);
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

        /// The scripted backend does not model the summariser; it returns
        /// a fixed summary so a byte-fallback compaction driven through it
        /// stays total rather than panicking.
        pub(super) fn summarize(&self, _model: &str) -> Result<SummaryOut, ProviderError> {
            Ok(SummaryOut {
                summary: "scripted summary".to_string(),
                usage: Usage::default(),
            })
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
    /// [`Engine::summarize`] hands to `from_genai`.
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
        assert_eq!(id.key_env(), Some("LOCAL_LLAMA_KEY"));
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
        assert_eq!(s, "rate limited: Weekly usage limit reached. Resets in 4 days.");
        assert!(!s.contains('{'), "summary must not leak the raw JSON body: {s}");
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
        let out = step_out_from_end("deepseek-v4-pro", end, true, AdapterKind::DeepSeek);
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
        let out = step_out_from_end("deepseek-v4-pro", end, true, AdapterKind::DeepSeek);
        assert!(
            !out.assistant_message
                .content
                .iter()
                .any(|p| matches!(p, ContentPart::ReasoningContent(_))),
            "no reasoning part when the provider surfaced none",
        );
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

    /// The cache suffix is omitted whenever there's nothing to say —
    /// whether cache activity is absent (`None`, as in `Usage::default`) or
    /// a real zero measurement (`Some(0)`, an Anthropic turn with no cache
    /// hit).  Both render `—`; they differ in intent, not on the wire.
    #[test]
    fn usage_display_omits_empty_cache_suffix() {
        assert_eq!(Usage::default().to_string(), "total 0 in / 0 out · —");
        let measured_zero = Usage {
            input: 100,
            output: 50,
            cache_creation: Some(0),
            cache_read: Some(0),
            dollars: 0.0,
            unmetered: false,
        };
        assert_eq!(measured_zero.to_string(), "total 100 in / 50 out · —");
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

    /// A stream that stalls before any response surfaces from `complete`'s
    /// before-request select as a `Transient` error — modelled here as
    /// `Attempt::Failed(Transient)`.  Driven through the real retry loop
    /// it must be re-issued up to `MAX_ATTEMPTS` and then surface `Err`
    /// (bounded, never hanging) so the turn fails cleanly instead of
    /// blocking forever on a dead socket.
    #[test]
    fn idle_timeout_before_token_is_retried_then_surfaced() {
        let calls = std::cell::Cell::new(0u32);
        let out: Result<(), ProviderError> = make_runtime().block_on(retry_with_backoff(
            "test",
            &cancel::Token::new(),
            async |_attempt| {
                calls.set(calls.get() + 1);
                Attempt::Failed(ProviderError::Transient {
                    cause: "stream idle: no response within timeout".into(),
                    attempts: 1,
                    body: None,
                })
            },
        ));
        assert_eq!(calls.get(), MAX_ATTEMPTS, "exhausts the transient budget");
        match out {
            Err(ProviderError::Transient { attempts, .. }) => {
                assert_eq!(attempts, MAX_ATTEMPTS, "surfaced with the attempt count")
            }
            other => panic!("expected Transient after the budget, got {other:?}"),
        }
    }

    /// Once visible text has streamed, a mid-stream stall is not retried:
    /// `complete` keeps the streamed prefix as a [`CutShort::Stalled`]
    /// [`StepOut`] so the session can commit it and continue the turn.
    /// `stalled_step_out` is that projection — a text-only assistant
    /// message, no tool calls, the stream cause carried for the note.  A
    /// real byte-level stall is a `reqwest` read-timeout we cannot
    /// construct in a unit test; a streaming 5xx classifies `Transient`
    /// identically, so it stands in as the cause here.
    #[test]
    fn stalled_step_out_keeps_streamed_prefix() {
        let cause = ProviderError::from_genai(
            &web_stream_http(StatusCode::SERVICE_UNAVAILABLE),
            "test-model",
        );
        assert!(
            matches!(cause, ProviderError::Transient { .. }),
            "the stand-in stall cause must classify Transient, got {cause:?}"
        );
        let brief = cause.brief();
        let step = stalled_step_out("test-model", "partial answer so far", &cause, false, AdapterKind::OpenAI);
        assert_eq!(
            step.cut_short,
            Some(CutShort::Stalled(brief)),
            "the stall cause rides along for the operator note",
        );
        assert!(step.tool_calls.is_empty(), "no tool call survives a stall");
        assert!(
            step.stop_reason.is_none(),
            "no End frame, so no stop reason"
        );
        assert!(
            step.assistant_message
                .content
                .first_text()
                .is_some_and(|t| t == "partial answer so far"),
            "the committed message is exactly the rendered prefix",
        );
    }

    /// The idle timeout is an idle (between-events) bound, not a total cap.
    /// Even the worst case — the full transient retry budget, each attempt
    /// idling for the whole timeout — leaves the total idle wait bounded and
    /// modest, so a genuinely stalled stream surfaces as a failed turn in
    /// minutes rather than appearing to hang.  This guards against a future
    /// bump of `STREAM_IDLE_TIMEOUT` (or `MAX_ATTEMPTS`) that would let the
    /// budget grow into an effective hang.
    #[test]
    fn idle_timeout_budget_stays_bounded() {
        let worst_case = STREAM_IDLE_TIMEOUT * MAX_ATTEMPTS;
        assert!(
            worst_case < Duration::from_secs(600),
            "a stalled stream must fail in minutes, not appear hung; idle budget is {worst_case:?}"
        );
    }
}