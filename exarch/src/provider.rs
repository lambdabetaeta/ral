//! LLM transport over genai: provider selection, the credential-keyed
//! transport cache, the streaming turn driver with its bounded retry loop,
//! and usage/cost accounting.
//!
//! `Provider::complete` takes already-rendered [`ChatMessage`]s and streams
//! one assistant reply; the transcript owns history, so the provider only
//! sends bytes and parses responses.  The provider-boundary error taxonomy
//! and the genai fault classifier live in the [`error`] submodule.

mod error;

pub use error::ProviderError;
pub(crate) use error::error_object;

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
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// The per-turn output cap is genai's per-model default (e.g.
// `AnthropicAdapter::resolve_max_tokens` picks 32k for Opus 4.x, 64k for
// Sonnet/Haiku/4-5) unless the user passes `--max-tokens`, which
// `Provider::max_tokens_override` carries as the `Some` that overrides it.

/// Retry budget for transient stream/network failures.  Three attempts
/// means up to two retries before surfacing the error.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
/// Stream-idle bound for retry attempts.  The first attempt gets the full
/// [`STREAM_IDLE_TIMEOUT`] — generous, so a slow high-effort
/// time-to-first-token is never mistaken for a stall.  But once an attempt
/// *has* idled out, the working hypothesis flips from "slow" to "broken",
/// and a retry that stalls the same way should fail fast rather than
/// re-spend the full budget: with keep-alive probes evicting dead
/// connections (see [`crate::tls::client`]) a healthy retry produces its
/// first event quickly, so a retry that is silent for a whole minute is
/// almost certainly stalled again.  Cuts the worst-case per-turn idle burn
/// from `3 × 180s` to `180s + 2 × 60s`.
const RETRY_IDLE_TIMEOUT: Duration = Duration::from_mins(1);

/// The stream-idle bound for the given 1-based attempt: the full
/// [`STREAM_IDLE_TIMEOUT`] on the first try, [`RETRY_IDLE_TIMEOUT`] on
/// retries (see there for why).
fn idle_timeout(attempt: u32) -> Duration {
    if attempt <= 1 {
        STREAM_IDLE_TIMEOUT
    } else {
        RETRY_IDLE_TIMEOUT
    }
}
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
    Gemini,
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
            Self::Gemini => ("gemini", "gemini-2.5-pro", "GEMINI_API_KEY"),
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
            Self::Openrouter | Self::OpencodeZen | Self::OpencodeGo | Self::Qwen => {
                AdapterKind::OpenAI
            }
            Self::Deepseek => AdapterKind::DeepSeek,
            Self::Gemini => AdapterKind::Gemini,
            Self::Xai => AdapterKind::Xai,
        }
    }

    /// Whether this provider bills as a flat subscription rather than per
    /// token. opencode Go is a flat $10/mo plan over an OpenAI-compatible
    /// gateway; opencode Zen on the same gateway is pay-as-you-go. A `ChatGPT`
    /// plan login is a flat subscription too, but it rides the OAuth
    /// credential rather than this `ProviderKind` property.
    pub fn flat_rate(self) -> bool {
        matches!(self, Self::OpencodeGo)
    }
}

/// An unusual provider declared in `config.ral`: a custom endpoint exarch
/// has no built-in knowledge of.
///
/// It carries the same four facts the rest of
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

/// A signed-in `ChatGPT` account, as a selectable provider identity.
///
/// A `ChatGPT` plan authorises over OAuth rather than an API key, and several
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
    /// The `OpenAI` account id this selection maps to.
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
/// signed-in `ChatGPT` account ([`ChatGptAccount`]).
///
/// This is the single abstraction credential resolution, model listing, and
/// transport building consume, so a custom provider — or a `ChatGPT` account —
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
    /// A signed-in `ChatGPT` account, authorising over OAuth.
    ChatGpt(Arc<ChatGptAccount>),
}

impl ProviderId {
    /// The stable label — `ProviderKind::info().0` for a famous provider, the
    /// config map key for a custom one, the account handle for a `ChatGPT` login.
    pub fn label(&self) -> &str {
        match self {
            Self::Famous(kind) => kind.info().0,
            Self::Custom(c) => &c.label,
            Self::ChatGpt(a) => &a.label,
        }
    }

    /// The environment variable this provider's API key is read from, or
    /// `None` for a provider that does not authenticate off an env key — a
    /// `ChatGPT` account authorises over OAuth, so it is loaded from the token
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
    /// always has one; a `ChatGPT` account redirects to the Codex backend
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
    /// wire protocol in the config and carries the resolved adapter; a `ChatGPT`
    /// account speaks the `OpenAI` Responses adapter to the Codex backend.
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
    /// not declare it, so this is a future slice's extension point. A `ChatGPT`
    /// account is unmetered too, but rides its OAuth credential (the bound
    /// `token_cell`) rather than this property — so it is not `flat_rate`.
    pub fn flat_rate(&self) -> bool {
        match self {
            Self::Famous(kind) => kind.flat_rate(),
            Self::Custom(_) | Self::ChatGpt(_) => false,
        }
    }

    /// The famous kind, when this is a built-in provider — the few sites that
    /// genuinely need the enum (the `OpenAI` per-model adapter refinement, the
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

/// How a provider's plan reads, for both metering and labelling. A turn
/// under either subscription flavour is unmetered; the flavour only changes
/// the decoration [`provider_label`] applies.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Subscription {
    /// A metered API key — turns are billed per token.
    Metered,
    /// A `ChatGPT` plan login authorised over OAuth.
    ChatGpt,
    /// A flat-rate plan declared by the provider's [`ProviderId`]
    /// (opencode Go's $10/mo gateway).
    FlatRate,
}

/// A provider's status/picker label: the subscription-decorated form when it
/// is on a plan, else the bare provider name. The single place the "decorate
/// when subscription" decision is made — and the only place the per-flavour
/// suffix is spelled — so the status bar, the `/model` switch, and the picker
/// rows cannot drift.
pub(crate) fn provider_label(subscription: Subscription, base: &str) -> String {
    match subscription {
        Subscription::Metered => base.to_string(),
        Subscription::ChatGpt => format!("{base} (ChatGPT subscription)"),
        Subscription::FlatRate => format!("{base} (subscription)"),
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
    /// Whether this turn's cost is off the meter: a `ChatGPT` plan login is
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
/// per-agent token tallies.
///
/// Under 10k prints in full (`0`, `9123`); from
/// 10k to <1M shows one decimal with a bare `.0` dropped (`46.6k`, but
/// `200k` not `200.0k`); 1M and over keeps one decimal (`1.0m`, `1.5m`).
/// One source of truth so the plain [`Display`] and the TUI's styled
/// renderers cannot drift (X9).
pub fn humanize_tokens(n: u64) -> String {
    // Tier on the rounded display, not the raw count: a value within ~50
    // tokens of 1M rounds to `1.0m`, never `1000k`.
    if n >= 999_950 {
        #[allow(clippy::cast_precision_loss, reason="display rounding of a token count; magnitude far below 2^52")]
        let m = n as f64 / 1_000_000.0;
        format!("{m:.1}m")
    } else if n >= 10_000 {
        #[allow(clippy::cast_precision_loss, reason="display rounding of a token count; magnitude far below 2^52")]
        let thousands = n as f64 / 1_000.0;
        let k = format!("{thousands:.1}");
        format!("{}k", k.strip_suffix(".0").unwrap_or(&k))
    } else {
        n.to_string()
    }
}

/// The humanised pieces of a usage line.
///
/// One source of truth for the
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
    /// The stream broke mid-response *after* partial text or reasoning had
    /// rendered: an idle stall, a transport error frame, or a close with no
    /// `End` frame.  Re-issuing would double-render the streamed prefix, so
    /// it is kept and the turn continues.  Carries the underlying cause for
    /// the note.
    Stalled(String),
}

/// One compaction summary response.
pub struct SummaryOut {
    pub summary: String,
    pub usage: Usage,
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
            () = wait_for_cancel(cancel) => return Err(ProviderError::Cancelled(cancel_site)),
            () = backoff_sleep(attempt, retry_after, max_delay_ms) => {}
        }
    }
}

/// The per-selection request tuning the `/model` overlay carries alongside the
/// model: the reasoning effort, the sampling temperature, and the
/// nucleus-sampling top-p.
///
/// Each is `None` for "auto" — the option is then
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
        let key = |e: &Option<ReasoningEffort>| e.as_ref().map(genai::chat::ReasoningEffort::variant_name);
        key(&self.effort) == key(&other.effort)
            && self.temperature == other.temperature
            && self.top_p == other.top_p
    }
}

/// The `extra_body` fragment that pins an `OpenRouter` request to a single
/// serving provider: `{"provider": {"order": ["<slug>"], "allow_fallbacks":
/// false}}`. `order` lists the provider in priority and `allow_fallbacks:false`
/// forbids routing elsewhere, so the request lands on exactly that upstream.
fn or_provider_extra_body(slug: &str) -> serde_json::Value {
    serde_json::json!({
        "provider": {
            "order": [slug],
            "allow_fallbacks": false,
        }
    })
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
    /// The chosen `OpenRouter` serving-provider slug for this selection, or `None`
    /// for "auto" (`OpenRouter` routes). Set by the `/model` overlay's provider
    /// control and restored from persisted state; folded into the request as
    /// `provider.order` routing by [`Engine::complete`], and only for an
    /// `OpenRouter` selection — it is inert on any other provider.
    route: Option<String>,
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

/// Identifies a cached transport by the three things that fix a genai client:
/// the credential's label, its secret (as a [`CredFingerprint`]), and the wire
/// `adapter`.  An `OpenAI` key spans both adapters — `gpt-4o` speaks chat
/// completions, `gpt-5` the Responses API — so they must not share one client
/// even though they share a credential; every other provider resolves a single,
/// model-independent adapter and so keeps one entry per (label, secret).
///
/// The secret fingerprint is what keeps the cache honest across a key rotation.
/// An API key's bearer is captured *by value* inside the resolver closures
/// [`build_client`] hands to genai, so the cached client holds the secret it was
/// built with.  Were the same label ever resolved with a different secret, a
/// label-and-adapter-only key would return that stale client.  Folding the
/// secret's fingerprint into the key makes a changed secret a natural cache miss
/// that rebuilds the client around the new one, while an unchanged secret keys
/// to the same entry and stays a cheap hit.  The OAuth path is secret-immune by
/// construction — it reads its token live from a shared cell per request — so it
/// fingerprints to a single constant rather than to the changing token.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TransportKey {
    cred: String,
    fingerprint: CredFingerprint,
    adapter: AdapterKind,
}

impl TransportKey {
    fn for_selection(id: &ProviderId, model: &str, cred: &Credential) -> Self {
        Self {
            cred: id.label().to_string(),
            fingerprint: CredFingerprint::of(cred),
            adapter: adapter_for_provider_model(id, model),
        }
    }
}

/// A non-secret fingerprint of a credential's secret, for keying the transport
/// cache only — never the secret itself, so it is safe to derive `Debug` and
/// carry in a logged key.  An API key fingerprints to a stable per-process hash
/// of its bearer string: equal keys hash equal (the common case, a cheap cache
/// hit), a rotated key hashes elsewhere (a rebuild).  An OAuth login carries no
/// baked secret — its client reads the token live from a shared cell every
/// request — so it fingerprints to a single constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum CredFingerprint {
    /// A non-reversible hash of an API key's bearer string.
    ApiKey(u64),
    /// The OAuth path: live-read per request, so secret-independent.
    OAuth,
}

impl CredFingerprint {
    fn of(cred: &Credential) -> Self {
        match cred {
            Credential::ApiKey(key) => {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                key.hash(&mut hasher);
                Self::ApiKey(hasher.finish())
            }
            Credential::OAuth(_) => Self::OAuth,
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
    /// The shared login cell when this credential authenticates off a `ChatGPT`
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
/// warmed beneath all Providers.
///
/// Each [`Provider`] holds the
/// [`Arc<Transport>`] it resolved at build time, so the map is touched only on
/// the cold warm path, never per request.
pub struct Engine {
    runtime: tokio::runtime::Runtime,
    /// Stable per-process identifier sent as `OpenAI`'s `prompt_cache_key` on
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
/// provider with a custom `endpoint` (`OpenRouter`) redirects the *service
/// target* through a [`ServiceTargetResolver`] that keeps the native
/// adapter and binds the key; with no endpoint the native adapter's
/// default target is used, gated by an [`AuthResolver`] that hands the key
/// only to that adapter.
///
/// genai's model-name heuristic falls back to local Ollama for unknown
/// names; the provider is the authority instead, so a misspelled `DeepSeek`
/// model fails at `DeepSeek` rather than being silently sent to
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
    let client = if let Some(base_url) = id.endpoint() {
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
    } else {
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
    };
    (client, adapter)
}

/// Build the genai client for a `ChatGPT` plan login. The `OpenAI` Responses
/// adapter renders the request body; an [`AuthResolver`] redirects every
/// request to the Codex backend with the login's bearer and account headers,
/// read live from `cell` so a token refreshed mid-session is picked up on the
/// next request without rebuilding the client.
fn build_oauth_client(cell: Arc<Mutex<oauth::OAuthToken>>) -> Client {
    let auth = AuthResolver::from_resolver_fn(move |iden: ModelIden| {
        if iden.adapter_kind == AdapterKind::OpenAIResp {
            let token = cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
        route: Option<String>,
    ) -> Self {
        let transport = engine.transport_for(id, &model, cred);
        Self {
            backend: Backend::Live { engine, transport },
            id: id.clone(),
            model,
            max_tokens_override,
            tuning,
            route,
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
            route: None,
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

    /// The `OpenRouter` serving-provider slug bound at build time, or `None` for
    /// auto — read by the `/model` overlay to seed the provider control, and
    /// folded into the request by [`Engine::complete`].
    pub fn route(&self) -> Option<&str> {
        self.route.as_deref()
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
    /// ([`provider_label`]). A `ChatGPT` plan login is the OAuth
    /// flavour (login cell present), an [`ProviderId::flat_rate`] provider
    /// the generic flavour; a scripted backend is never a subscription.
    pub fn subscription(&self) -> Subscription {
        match &self.backend {
            Backend::Live { transport, .. } => transport.subscription(),
            Backend::Scripted(_) => Subscription::Metered,
        }
    }

    /// Stream one assistant response for an already-rendered message list.
    ///
    /// Retries with bounded exponential backoff on `Transient` and
    /// `RateLimited` errors **only when no text or reasoning has streamed
    /// this attempt** — once anything has flowed to `on_text` or `on_think`
    /// the UI has committed to a partial render, and re-streaming the
    /// request would double output.
    /// Run one provider round-trip, streaming assistant text through
    /// `on_text`.  `cancel` is the *request-local* cancellation handle: the
    /// foreground turn passes its root token (linked to the signal slot, so
    /// Esc cancels it), and an async agent passes its registry token (so
    /// `agent_cancel` / `/clear` / the worker ceiling cancel it without
    /// touching the foreground request).  Two concurrent requests no longer
    /// share the one process-global slot.
    pub(crate) fn complete<F: FnMut(&str), G: FnMut(&str)>(
        &self,
        system: &str,
        messages: Vec<ChatMessage>,
        tools: &[&'static dyn crate::tools::Tool],
        on_text: &mut F,
        on_think: &mut G,
        cancel: &cancel::Token,
    ) -> Result<StepOut, ProviderError> {
        match &self.backend {
            Backend::Live { engine, transport } => engine.complete(
                transport,
                &self.model,
                self.max_tokens_override,
                &self.tuning,
                self.openrouter_route(),
                system,
                messages,
                tools,
                on_text,
                on_think,
                cancel,
            ),
            Backend::Scripted(s) => s.complete(&self.model, on_text, on_think),
        }
    }

    /// The serving-provider slug to route through, but only for an `OpenRouter`
    /// selection — `provider.order` is `OpenRouter`'s wire contract, meaningless
    /// elsewhere, so a stray route on any other provider is dropped rather than
    /// injected into a body that would not understand it.
    fn openrouter_route(&self) -> Option<&str> {
        match self.id.famous() {
            Some(ProviderKind::Openrouter) => self.route.as_deref(),
            _ => None,
        }
    }

    /// Summarise already-rendered transcript messages.
    ///
    /// Retries unconditionally on Transient/RateLimited — there is no
    /// streamed side effect to worry about.  `cancel` is the request-local
    /// handle, as for [`Provider::complete`].
    ///
    /// # Errors
    /// Returns `Err` if the summarisation round-trip fails (after retrying
    /// transient and rate-limited responses).
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
    /// model only seeds the adapter pin (`OpenAI` chat vs. responses); every
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
    /// credential is metered; a flat subscription — a `ChatGPT` plan login
    /// ([`Self::token_cell`]) or an [`ProviderId::flat_rate`] plan
    /// ([`Self::flat_rate`]) — is unmetered.
    fn metered(&self) -> bool {
        self.token_cell.is_none() && !self.flat_rate
    }

    /// How this transport's plan reads — see [`Provider::subscription`].
    fn subscription(&self) -> Subscription {
        if self.token_cell.is_some() {
            Subscription::ChatGpt
        } else if self.flat_rate {
            Subscription::FlatRate
        } else {
            Subscription::Metered
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
    /// on first use.  One client per (credential label, secret, wire adapter),
    /// deduped by [`TransportKey`], so a `/model` re-selection onto an
    /// already-warmed key — or a new agent on it — reuses the client, while a
    /// rotated secret under the same label keys to a fresh entry and rebuilds
    /// (see [`TransportKey`]).  Touched only on this cold warm path; the hot
    /// path holds the returned `Arc` directly.
    fn transport_for(&self, id: &ProviderId, model: &str, cred: &Credential) -> Arc<Transport> {
        self.transports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(TransportKey::for_selection(id, model, cred))
            .or_insert_with(|| Transport::build(id, model, cred))
            .clone()
    }

    /// Renew the `ChatGPT` access token when it is near expiry, so the request
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
            let token = cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
                *cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = fresh;
            }
            Err(e) => eprintln!("exarch: ChatGPT token refresh failed: {e}"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn complete<F: FnMut(&str), G: FnMut(&str)>(
        &self,
        transport: &Transport,
        model: &str,
        max_tokens_override: Option<u32>,
        tuning: &Tuning,
        route: Option<&str>,
        system: &str,
        messages: Vec<ChatMessage>,
        tools: &[&'static dyn crate::tools::Tool],
        on_text: &mut F,
        on_think: &mut G,
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
        // Override genai's per-model output cap only when the user asked
        // with `--max-tokens`; otherwise the adapter's default stands.
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
        // OpenRouter provider routing: pin the chosen serving provider and
        // forbid fallback, so the request lands on exactly the upstream the
        // user picked. Folded in as `extra_body` — the OpenAI adapter merges it
        // into the request body, which is how OpenRouter reads the field.
        if let Some(slug) = route {
            options = options.with_extra_body(or_provider_extra_body(slug));
        }

        let step = self.runtime.block_on(retry_with_backoff(
            "before request",
            cancel,
            async |attempt| {
                let mut req = req_template.clone();
                // Chat mode advertises no tools; send no `tools` field at all
                // rather than an empty array, which some backends reject.
                req.tools = (!tool_defs.is_empty()).then(|| tool_defs.clone());
                let mut seen_streamed_content = false;
                // Mirrors what `on_text` rendered, so a committed stall can
                // commit exactly the prefix the user already saw.
                let mut streamed = String::new();
                // Mirrors what `on_think` rendered, for the same reason: a
                // stall deep into a thinking block must not discard
                // reasoning the model already produced and the user already
                // watched stream in.
                let mut streamed_reasoning = String::new();
                let attempt_result: Result<StreamEnd, ProviderError> = async {
                    let mut resp = tokio::select! {
                        biased;
                        () = wait_for_cancel(cancel) => {
                            return Err(ProviderError::Cancelled("before request"));
                        }
                        // A fresh `sleep` per select entry bounds the connect
                        // phase — which `tls::client`'s `read_timeout` does
                        // not cover — and time-to-first-event; surfaced as a
                        // transient transport error so the retry budget
                        // re-issues it (no token has streamed yet).  Retries
                        // get the shorter bound — see [`idle_timeout`].
                        () = tokio::time::sleep(idle_timeout(attempt)) => {
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
                        // After the response opens, liveness belongs to the
                        // transport, not to semantic output.  Anthropic may
                        // keep a long-thinking stream alive with SSE pings
                        // that genai consumes without yielding a decoded
                        // `ChatStreamEvent`; timing out this `next()` would
                        // punish a live model for being quiet.  Raw silence is
                        // still bounded by `tls::client`'s per-read timeout,
                        // which genai surfaces as a stream error below.
                        let event = tokio::select! {
                            biased;
                            () = wait_for_cancel(cancel) => {
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
                            ChatStreamEvent::Chunk(c) => {
                                seen_streamed_content |= !c.content.is_empty();
                                streamed.push_str(&c.content);
                                on_text(&c.content);
                            }
                            ChatStreamEvent::End(end) => return Ok(end),
                            ChatStreamEvent::ReasoningChunk(c) => {
                                seen_streamed_content |= !c.content.is_empty();
                                streamed_reasoning.push_str(&c.content);
                                on_think(&c.content);
                            }
                            // Thought signatures and tool calls are captured
                            // whole in the `End` frame and replayed from
                            // there by `step_out_from_end`; a stall before
                            // `End` has no signature or tool call to salvage
                            // — unlike text and reasoning, which stream
                            // incrementally and are worth keeping.  Matched
                            // explicitly rather than with a wildcard so a
                            // new genai stream variant fails the build
                            // instead of vanishing (X10).
                            ChatStreamEvent::Start
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
                    Ok(end) => {
                        Attempt::Done(step_out_from_end(model, end, transport.metered(), adapter))
                    }
                    // A cancel is surfaced as-is regardless of streamed tokens.
                    Err(e @ ProviderError::Cancelled(_)) => Attempt::Failed(e),
                    // Visible text or reasoning already streamed, then the
                    // stream broke: a re-issue would double-render, so
                    // commit the streamed prefix as a cut-short turn and let
                    // the session continue it (mirrors the output-cap
                    // truncation path) rather than discarding the work and
                    // ending the run.
                    Err(e) if seen_streamed_content => Attempt::Done(stalled_step_out(
                        model,
                        &streamed,
                        &streamed_reasoning,
                        &e,
                        transport.metered(),
                        adapter,
                    )),
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
            async |attempt| {
                let req = req_template.clone();
                let r = tokio::select! {
                    biased;
                    () = wait_for_cancel(cancel) => {
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
                    // `Committed`.  Retries get the shorter bound — see
                    // [`idle_timeout`].
                    () = tokio::time::sleep(idle_timeout(attempt)) => {
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

/// Stamp the final attempt count onto a `Transient` error before
/// surfacing it — the one variant carrying an `attempts` field.  Every
/// other variant (`RateLimited` included) passes through unchanged.
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
/// on the next request.  `DeepSeek`'s thinking mode *requires*
/// `reasoning_content` to be echoed back on any assistant turn that
/// precedes tool results — dropping it returns a 400 ("the
/// `reasoning_content` in the thinking mode must be passed back to the
/// API").  genai's `OpenAI` adapter emits the field only when the message
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
/// visible text or reasoning streamed (and was already rendered) before the
/// stream stalled, errored, or closed without an `End` frame, so re-issuing
/// would double-render: the streamed prefix becomes an assistant message
/// carrying whatever text and reasoning arrived, marked
/// [`CutShort::Stalled`], which the session commits and continues from.  A
/// reasoning-only stall (no text yet) still surfaces its reasoning through
/// [`StepOut::reasoning`] — read by the caller before `admit_assistant` may
/// judge the message itself not substantive and stub it — so the trace is
/// not silently lost even though it cannot stand alone as a renderable
/// turn.  No `End` frame arrived, so there is no usage to meter and no tool
/// call to capture.
fn stalled_step_out(
    model: &str,
    streamed: &str,
    streamed_reasoning: &str,
    cause: &ProviderError,
    metered: bool,
    adapter: AdapterKind,
) -> StepOut {
    let reasoning = (!streamed_reasoning.is_empty()).then(|| streamed_reasoning.to_string());
    StepOut {
        assistant_message: ChatMessage::assistant(streamed.to_string())
            .with_reasoning_content(reasoning.clone()),
        tool_calls: Vec::new(),
        reasoning,
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
/// tool-use loop pays `cache_read` rate on the prefix and `cache_creation`
/// only on the delta).
///
/// Anthropic honours these breakpoints directly.  `OpenAI` auto-caches any
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
/// caches off `prompt_cache_key` and ignores message-level `cache_control`,
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
/// model's price card for `adapter`.  Used by both the streaming turn path
/// (via `step_out_from_end` / `stalled_step_out`) and the compaction
/// summary (`Engine::summarize`).
fn usage_from(model: &str, raw: &genai::chat::Usage, metered: bool, adapter: AdapterKind) -> Usage {
    let input = u64::try_from(raw.prompt_tokens.unwrap_or(0)).unwrap_or(0);
    let output = u64::try_from(raw.completion_tokens.unwrap_or(0)).unwrap_or(0);
    // Preserve the `None` vs `Some(0)` distinction: OpenAI-family
    // adapters leave `cache_creation_tokens` unset because the upstream
    // API does not report writes, and the renderer needs that signal so
    // it can show `—` instead of a misleading `0 wr`.
    let cache_creation = raw
        .prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cache_creation_tokens)
        .map(|n| u64::try_from(n).unwrap_or(0));
    let cache_read = raw
        .prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cached_tokens)
        .map(|n| u64::try_from(n).unwrap_or(0));
    let dollars = if metered {
        crate::pricing::lookup_for(model, adapter)
            .map_or(0.0, |p| {
                p.dollars(
                    input,
                    output,
                    cache_creation.unwrap_or(0),
                    cache_read.unwrap_or(0),
                )
            })
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
/// [`crate::agent::Agent::apply`] end-to-end with no network.
///
/// A
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

        /// An empty assistant turn — no text, no tool calls, `end_turn` —
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
        ///
        /// # Panics
        /// Panics if the script mutex is poisoned.
        pub fn then(self, reply: Reply) -> Self {
            self.completes.lock().unwrap().push_back(reply);
            self
        }

        pub(super) fn complete<F: FnMut(&str), G: FnMut(&str)>(
            &self,
            _model: &str,
            on_text: &mut F,
            _on_think: &mut G,
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
        #[allow(
            clippy::unused_self,
            clippy::unnecessary_wraps,
            reason = "scripted backend arm of the `Backend` dispatch in `Provider::summarize`; its `&self` receiver and `Result` return mirror the live engine's method so both match arms share one shape."
        )]
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
    use reqwest::StatusCode;

    /// A `ModelIden` for the stream-fault fixture below.  The classifier
    /// never reads it, so the adapter kind is arbitrary.
    fn iden(model: &str) -> ModelIden {
        ModelIden::new(AdapterKind::Anthropic, model)
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
    /// verbatim — the `OpenAI` per-model refinement applies only to the
    /// famous `OpenAI` kind, never to a custom OpenAI-compatible endpoint.
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

    /// The transport key folds in the credential's secret: the same API key
    /// keys to the same entry (a cheap cache hit, the common case), a rotated
    /// key under the same provider label keys elsewhere (a cache miss that
    /// rebuilds the client around the new secret rather than serving the stale
    /// one captured in the resolver closures).
    #[test]
    fn transport_key_separates_rotated_api_keys() {
        let id = ProviderId::Famous(ProviderKind::Anthropic);
        let key = |s: &str| {
            TransportKey::for_selection(&id, "claude-opus-4", &Credential::ApiKey(s.into()))
        };
        assert_eq!(
            key("sk-original"),
            key("sk-original"),
            "an unchanged secret must key to the same cached transport"
        );
        assert_ne!(
            key("sk-original"),
            key("sk-rotated"),
            "a rotated secret must key to a fresh entry, never the stale client"
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

    /// `DeepSeek` thinking mode rejects a follow-up request unless the prior
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
    /// non-thinking providers (and `DeepSeek`'s non-thinking models) clean.
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

    /// An unmetered turn (a `ChatGPT` plan login) reports its cost slot as
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

    /// `AddAssign` keeps the `Some` flavour once any turn has populated
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
        assert_eq!(acc.cache_creation, Some(42), "None + Some promotes to Some");
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
                assert_eq!(attempts, MAX_ATTEMPTS, "surfaced with the attempt count");
            }
            other => panic!("expected Transient after the budget, got {other:?}"),
        }
    }

    /// Once visible text has streamed, a mid-stream stall is not retried:
    /// `complete` keeps the streamed prefix as a [`CutShort::Stalled`]
    /// [`StepOut`] so the session can commit it and continue the turn.
    /// `stalled_step_out` is that projection — an assistant message carrying
    /// the streamed text, no tool calls, the stream cause carried for the
    /// note.  A real byte-level stall is a `reqwest` read-timeout we cannot
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
        let step = stalled_step_out(
            "test-model",
            "partial answer so far",
            "",
            &cause,
            false,
            AdapterKind::OpenAI,
        );
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
        assert!(
            step.reasoning.is_none(),
            "no reasoning streamed, so none is carried",
        );
    }

    /// A stall that hits while the model is still deep in a thinking block —
    /// before any visible text — must not discard that reasoning.  This is
    /// the case a bare `seen_any_token` gated on text chunks alone would
    /// miss entirely: the whole attempt would surface as a clean `Failed`
    /// with the reasoning gone, indistinguishable from a stall on the very
    /// first byte.
    #[test]
    fn stalled_step_out_keeps_streamed_reasoning_even_without_text() {
        let cause = ProviderError::from_genai(
            &web_stream_http(StatusCode::SERVICE_UNAVAILABLE),
            "test-model",
        );
        let step = stalled_step_out(
            "test-model",
            "",
            "let me work through the cases",
            &cause,
            false,
            AdapterKind::OpenAI,
        );
        assert_eq!(
            step.reasoning.as_deref(),
            Some("let me work through the cases"),
            "reasoning streamed before the stall must survive it",
        );
        assert!(
            step.assistant_message
                .content
                .iter()
                .any(|p| matches!(p, ContentPart::ReasoningContent(r) if r == "let me work through the cases")),
            "the reasoning also rides on the assistant message, mirroring step_out_from_end",
        );
    }

    /// The explicit idle timeout now applies before a streaming response has
    /// opened (and to non-streaming summary requests), not between decoded
    /// stream events.  Even the worst case — the full transient retry budget,
    /// each opening attempt idling for its whole per-attempt bound — leaves
    /// that pre-stream wait bounded and modest, while an already-open stream
    /// uses the transport's byte-level read timeout for raw silence.
    #[test]
    fn idle_timeout_budget_stays_bounded() {
        let worst_case: Duration = (1..=MAX_ATTEMPTS).map(idle_timeout).sum();
        assert!(
            worst_case < Duration::from_mins(10),
            "a stalled stream must fail in minutes, not appear hung; idle budget is {worst_case:?}"
        );
        assert_eq!(
            idle_timeout(1),
            STREAM_IDLE_TIMEOUT,
            "first attempt gets the full bound"
        );
        assert!(
            idle_timeout(2) < STREAM_IDLE_TIMEOUT,
            "retries fail faster than the first attempt"
        );
    }

    /// The provider-routing fragment pins exactly the chosen slug and forbids
    /// fallback — the shape `OpenRouter` reads off the request body.
    #[test]
    fn or_provider_extra_body_pins_slug_without_fallback() {
        assert_eq!(
            or_provider_extra_body("deepinfra"),
            serde_json::json!({
                "provider": { "order": ["deepinfra"], "allow_fallbacks": false }
            })
        );
    }

    /// The single source of truth for the subscription decoration: a metered
    /// provider keeps its bare name, a `ChatGPT` login carries the `OpenAI` plan
    /// suffix, and a flat-rate plan the generic one — so the status bar, the
    /// `/model` switch, and the picker cannot drift across flavours.
    #[test]
    fn provider_label_decorates_per_flavour() {
        assert_eq!(
            provider_label(Subscription::Metered, "deepseek"),
            "deepseek"
        );
        assert_eq!(
            provider_label(Subscription::ChatGpt, "openai"),
            "openai (ChatGPT subscription)"
        );
        assert_eq!(
            provider_label(Subscription::FlatRate, "opencode-go"),
            "opencode-go (subscription)"
        );
    }
}
