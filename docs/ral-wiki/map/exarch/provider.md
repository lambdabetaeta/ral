---
generated_at_commit: 25e66c1
generated_at_date: 2026-07-03
covers_paths: [exarch/src/provider.rs, exarch/src/pricing.rs]
---

# Map: exarch / provider

`provider.rs` is the LLM transport — a wrapper over the `genai` crate. The
transcript owns history; the `Provider` only sends bytes and parses replies.

## Provider identity

A *`ProviderId`* is the single abstraction credential resolution, model
listing, and transport building consume, so every kind of provider flows
through the same machinery. **Identity is keyed on the label alone** — a
provider's label is its unique key in the credential store, the model catalog,
and the `/model` picker — and three arms supply it:

- `Famous(ProviderKind)` — a built-in provider auto-discovered from its
  conventional key env var (`ProviderKind::info` gives `(label, default_model,
  key_env)`; `endpoint`, `default_adapter`, and `flat_rate` give the rest).
  Eight kinds: Anthropic, OpenAI, OpenRouter, DeepSeek, opencode Zen, opencode
  Go, xAI, Qwen. See [[decisions/260613_provider-config-ral-script|provider-config-ral-script]].
- `Custom(Arc<CustomProvider>)` — an *unusual provider* declared in a
  hand-written `config.ral`: a custom endpoint exarch has no built-in
  knowledge of. It carries the same four facts as a famous kind — label,
  key env, endpoint, wire adapter — but as owned runtime data rather than the
  `'static` table baked into the enum, with the protocol mapped onto genai's
  `AdapterKind` at decode time. Slice 3 of [[decisions/260613_provider-config-ral-script|provider-config-ral-script]].
- `ChatGpt(Arc<ChatGptAccount>)` — a signed-in ChatGPT account, authorising
  over OAuth. **Each account is its own selectable identity**, so switching
  accounts *is* switching the selected provider — no second selection
  dimension. It holds only the account label and id; the live tokens live in
  the [[map/exarch/agent|credential]] store's `OAuth` cell, not here.

**The flat-rate vs OAuth split is two distinct unmetered axes.** A subscription
turn carries no per-token price, and a provider reaches that state two ways:
opencode Go is a flat $10/mo gateway flagged by `ProviderKind::flat_rate`,
while a ChatGPT plan rides its OAuth login cell instead — so a ChatGPT account
is *not* `flat_rate`. `Live::metered` is false when either holds.

## Building the transport

`build_client` binds the resolved `Credential` to a genai `Client`:

- An **API key** keeps the provider's native adapter (it fixes the wire
  format, so an Anthropic provider speaks Anthropic even at a custom base URL).
  A custom `endpoint` redirects the *service target* through a
  `ServiceTargetResolver`; with no endpoint the native default target is used,
  gated by an `AuthResolver` that hands the key only to that adapter. genai's
  unknown-name fallback to local Ollama is overridden — the provider is the
  authority, so a misspelled model fails at its provider rather than silently
  hitting `localhost`.
- An **OAuth** login branches to `build_oauth_client`: the Responses adapter
  renders the body, and an `AuthResolver` redirects every request to the Codex
  backend with the login's bearer and account headers, read live from the
  shared cell so a mid-session refresh is picked up without rebuilding.
  `refresh_if_stale` renews the token before a request when it is near expiry,
  upserting just that account's entry.

## The streaming and summary paths

- `complete(system, messages, advertise_root_only, on_text, on_think, cancel)` —
  streams one assistant reply, calling `on_text` for text and `on_think` for
  reasoning, and projects the `StreamEnd` into a `StepOut` (assistant message,
  tool calls, reasoning, `Usage`, `StopReason`). It preserves
  `reasoning_content` on the assistant message so thinking mode round-trips.
  The stream-event match is exhaustive — thought-signature and tool-call chunks
  are captured in the `End` frame and replayed by `step_out_from_end`, so they
  are dropped by a *named* arm, never a wildcard, and a new genai stream variant
  fails the build (X10).
- A **pre-stream idle timeout** bounds request open and time-to-response; once a
  streaming response is open, exarch does **not** time out the gap between
  decoded `ChatStreamEvent`s. Liveness is raw transport progress: reqwest's
  per-read `STREAM_IDLE_TIMEOUT` (180s) turns true byte-level silence into a
  retryable stream error, while provider heartbeats or SSE pings that genai
  consumes below the semantic event layer can keep a long-thinking model alive.
  This lands the first local slice of
  [[decisions/260702_provider-heartbeats-and-retry-boundaries|provider-heartbeats-and-retry-boundaries]].
- `summarize` — one non-streamed call producing a compaction summary; used by
  [[map/exarch/agent|`Agent::compact`]]. The same idle timeout bounds the
  whole `exec_chat` request (no incremental events to idle between). A summary
  that itself hit the 1024-token budget is surfaced as `Truncated`, so
  `compact` keeps the un-summarised history rather than committing a half
  summary (X10).
- `cancel` is the **request-local** cancellation handle: the foreground turn
  passes its root token (Esc-linked), an async `agent` passes its registry
  token, so two concurrent requests no longer share one process-global slot —
  the provider-side seam of [[decisions/260617_async-agent-tool|async-agent-tool]];
  the registry and inbox belong to [[map/exarch/tools|tools]] and
  [[map/exarch/agent|agent]].

## Retry driver

Both paths run on a tokio runtime through **one retry driver**,
`retry_with_backoff` over an `Attempt<T>` (`Done` / `Failed`):

- The streaming-specific rule rides on `Done`: once text or reasoning has
  flowed to `on_text`/`on_think` the UI has committed to a partial render, so a
  re-issue that would double output is *not* retried — `stalled_step_out`
  projects the streamed prefix and reasoning into a `CutShort::Stalled`
  `StepOut` returned as `Attempt::Done`, and the session commits it. No third
  "don't retry" variant is needed.
- Rate limits get a larger budget and a higher backoff ceiling than transient
  failures (`retry_limits`), and an explicit `retry-after` is honoured.

Transport retry lives here, so the [[map/exarch/agent|nudge]] rules cover
only model-behaviour outcomes, not transport.

## Structural error classification

`ProviderError::from_genai` **reads retryability from genai's typed variants,
not from its `Display` string.** A single structural walk, `Fault::of`, descends
each error to one of three leaves — `Status` (a non-2xx HTTP response), `Transport`
(a `reqwest` fault with no status), or `Terminal` (no recoverable leaf) — recovering
the `StatusCode`, the response `HeaderMap`, and the parsed JSON body across the four
paths a non-2xx reaches us by: `HttpError`, `WebModelCall(ResponseFailedStatus)`,
the `HttpError` boxed inside a streaming `WebStream` (recursion), and a mid-stream
`ChatResponse` frame whose code lives in `body["error"]["code"]` / `body["code"]`.
The status drives the `RateLimited` (429) / `Transient` (5xx) / `Api` (other 4xx)
split; a `retry-after` header is read directly when carried. The `_ => Terminal`
floor makes the walk total — a contract breach (a non-JSON 2xx) or an unrecognised
shape surfaces raw rather than being retried on a `Display`-string guess. The full
tutorial is [[internals/provider-fault-recovery|provider-fault-recovery]]
([[invariants/transcript-admission|transcript-admission]]).

Each retryable and 4xx variant carries the parsed body as
`Option<serde_json::Value>` to the boundary, so the renderer can print a
labelled, structured error from the JSON rather than scraping the cause text;
the chrome lives in [[map/exarch/cards|cards]] / [[map/exarch/frontend|frontend]].
`parse_retry_after` slices the *lowercased* copy it searches, never indexing
the original with an offset taken from it, so a length-changing lowercase (`İ`)
cannot land mid-character and panic (X8).

## Usage, pricing, and the token formatter

- `humanize_tokens` is the **one rule every token readout shares** — the usage
  line, the startup banner's context/limit fields, and the per-agent token
  tallies — so the plain `Display` and the styled TUI renderers cannot drift
  on what a turn cost (X9).
- `Usage::parts` (`UsageParts`) is the single content/layout source for the
  usage line: `Display` joins the pieces as text and the
  [[map/exarch/frontend|TUI]] styles them. An `unmetered` turn (a flat
  subscription) renders its cost slot as `subscription` rather than a price;
  the token counts still render.
- `pricing.rs` fetches OpenRouter's `/api/v1/models` catalog once per process
  and caches it; `ModelPricing::dollars` strips the cache counts out of `input`
  and bills uncached/cache-creation/cache-read/output each at its rate, falling
  back to the base input rate when no separate cache rate is published. The
  catalog backs every provider (OR republishes upstream cards) and also
  supplies `ModelCaps` (context window, canonical slug) for the startup banner.
  Offline starts degrade to `—`.

`build_cached_request` sets two `cache_control: ephemeral` breakpoints (system
+ tools, growing transcript) for the message-based adapters, and a per-process
`prompt_cache_key` for OpenAI shard routing. The Responses adapter is the
exception: its system prompt rides the top-level `instructions` field, since a
`System` message would leave `instructions` empty and the Codex backend rejects
that.
