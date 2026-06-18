---
generated_at_commit: 7ba500b
generated_at_date: 2026-06-17
covers_paths: [exarch/src/provider.rs, exarch/src/pricing.rs]
---

# Map: exarch / provider

`provider.rs` is the LLM transport — a wrapper over the `genai` crate. The
`SessionLog` owns history; the `Provider` only sends bytes and parses replies.

- `complete(system, messages, on_text)` — streams one assistant reply, calling
  `on_text` per token, and projects the `StreamEnd` into a `StepOut`
  (assistant message, tool calls, `Usage`, `StopReason`). It preserves
  `reasoning_content` on the assistant message so DeepSeek thinking mode
  round-trips. The stream-event match is exhaustive — the reasoning, thought-
  signature, and tool-call chunks are captured in the `End` frame and replayed
  by `step_out_from_end`, so they are dropped here by a *named* arm, never a
  wildcard, and a new genai stream variant fails the build (X10).
- `summarize` — one non-streamed call producing a compaction summary; used by
  [[map/exarch/session|`Session::compact`]]. A summary that itself hit the
  1024-token budget is surfaced as `Truncated`, so `compact` keeps the
  un-summarised history rather than committing a half summary (X10).

Both run on a tokio runtime through **one retry driver**, `retry_with_backoff`
over an `Attempt<T>` (`Done` / `Failed` / `Committed`).

- `Committed` is the streaming-specific rule: once any token has flowed to
  `on_text` the UI has committed to a partial render, so a re-issue that would
  double tokens is surfaced immediately rather than retried.
- `ProviderError::from_genai` classifies genai's `Display` text — substring-based
  on purpose, since genai's internal error variants are not exposed — into
  `Cancelled` / `Transient` / `RateLimited` / `Api` / `Truncated` / `Other`.
  `parse_4xx_status` matches both the `status: <code>` token and a JSON-body
  `"code": <code>` (the OpenRouter mid-stream shape), so a `{"code":400}` body
  classifies as `Api` rather than opaque `Other`
  ([[invariants/transcript-admission|X3]]). `parse_4xx_status` /
  `parse_retry_after` slice the *lowercased* copy they search, never index the
  original with an offset taken from it — a length-changing lowercase (`İ`)
  would otherwise land mid-character and panic (X8).
- Rate limits get a larger budget and a higher backoff ceiling than transient
  failures (`retry_limits`); honours a parsed `retry-after`.

Transport retry lives here, so the [[map/exarch/session|nudge]] rules cover only
model-behaviour outcomes, not transport.

`Usage::parts` (`UsageParts`) is the single content/layout source for the usage
line: the plain `Display` joins the pieces as text and the
[[map/exarch/frontend|TUI]]'s `usage_text` styles them, so the chrome and the
logs cannot drift on what a turn cost (X9). The TUI `ctx N%` gauge reads genai's
`prompt_tokens` directly — it already folds the cache-read/creation counts in, so
adding them again double-counted the gauge (X4).

`ProviderKind` maps each provider to `(label, default_model, key_env)`, an
endpoint (OpenRouter), and a genai `AdapterKind`. `build_cached_request` sets
two `cache_control: ephemeral` breakpoints (system+tools, growing transcript)
for Anthropic, and a per-process `prompt_cache_key` for OpenAI shard routing.

`pricing.rs` fetches OpenRouter's `/api/v1/models` catalog once per process and
caches it; `ModelPricing::dollars` splits cached/uncached input and bills each
turn. The catalog backs every provider (OR republishes upstream cards), and
also supplies `ModelCaps` (context window, canonical slug) for the startup
banner. Offline starts degrade to `—`.
