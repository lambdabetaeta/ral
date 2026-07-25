---
verified_at_commit: f7cf93a
verified_at_date: 2026-07-25
anchors: [from_genai, Fault, of_webc, of_boxed, of_reqwest, ProviderError, RateLimited, Transient, Api, Truncated, retry_with_backoff, Attempt, retry_limits, backoff_sleep, parse_retry_after, retry_after_header, json_status_code, parse_json_body, stalled_step_out, STREAM_IDLE_TIMEOUT, MAX_ATTEMPTS, RATE_LIMIT_MAX_ATTEMPTS]
---

# Provider faults and recovery

**A request to a language model fails in a dozen shapes — a refused 4xx, an
overloaded 5xx, a 429 asking us to wait, a socket that drops mid-token, a
gateway that returns HTML where JSON was promised — and exactly three things
can be done about any of them: retry it, surface it to the user, or commit the
partial work we already showed.** The provider's `error`, `retry`, and `stream`
modules collapse the dozen shapes onto those three responses with a single
rule: *read the recovery from the error's typed structure, never from its
`Display` string.* This page walks the path a genai error takes from the wire
to a recovery decision.

The transport itself — identity, client building, the streaming and summary
calls — is [[map/exarch/provider|the provider map]]; this is the failure half.
It is a different discipline from ral's own [[design/failure|failure model]]:
there, *failure is a status that propagates*; here, a fault is a *transport
outcome we classify and recover*, never a value.

## What genai hands us

Every fault arrives as one `genai::Error`. The variants that matter for
recovery carry the truth in different places, and the first job is knowing
where each keeps it:

| genai variant | where it comes from | what carries the verdict |
|---|---|---|
| `HttpError { status, body }` | a raw HTTP-level failure | a typed `StatusCode` |
| `WebModelCall` / `WebAdapterCall { webc_error }` | the non-streamed `exec_chat` call (compaction) and adapter model-list calls | a `webc::Error` (below) |
| `WebStream { error: BoxError }` | the streaming `exec_chat_stream` path | a *boxed* leaf — see below |
| `ChatResponse { body }` | a mid-stream SSE error frame | a status code *inside the JSON*, at `body["error"]["code"]` or `body["code"]` |
| everything else | bad request, auth gap, mapping failure, stream-parse error | nothing to recover |

The `webc::Error` inside a `WebModelCall` narrows again:

- `ResponseFailedStatus { status, headers, body }` — a non-2xx response; the
  `headers` retain `Retry-After`.
- `Reqwest(e)` — a transport-level `reqwest::Error` with no status.
- `ResponseFailedNotJson` / `ResponseFailedInvalidJson` — a **2xx** whose body
  was not the JSON genai required. (These fire *only* on success status; a
  non-2xx is already a `ResponseFailedStatus`.)

The streaming `WebStream` is the subtle one: its `error` is a
`Box<dyn Error>`, and genai boxes exactly three things into it — a
`genai::Error::HttpError` (the initial response was non-2xx), a `reqwest::Error`
(the socket failed mid-body), or a `FromUtf8Error` (the bytes were corrupt).
Nothing else. That closed set is what lets the classification stay structural.

## The structural walk: `Fault`

Rather than ask "is this retryable?" of each variant ad hoc, the classifier
distils every genai error down to one of three *recovery leaves*. That
distillation is `Fault`:

```
enum Fault<'a> {
    Status { status: StatusCode, headers: Option<&'a HeaderMap>, body: Option<Value> },
    Transport,
    Terminal,
}
```

- **`Status`** — a completed HTTP response reached us with a non-2xx code. The
  code, the headers (for `Retry-After`), and the provider's JSON error body
  ride along; the *caller* decides what a given status means.
- **`Transport`** — a `reqwest` fault with no status: connect, timeout, a
  malformed request, or a body that dropped or failed to decode mid-flight.
  Retryable by nature — nothing reached the model.
- **`Terminal`** — no status and no transport leaf: a request built wrong, an
  auth gap, a contract breach, a parse corruption. Retrying only re-loses.

`Fault::of` is the walk. It descends the typed tree to the leaf and never
touches the `Display` text:

- `HttpError` → `Status` directly.
- `WebModelCall` / `WebAdapterCall` → `of_webc`: `ResponseFailedStatus` →
  `Status` (with headers), `Reqwest` → `of_reqwest`, anything else →
  `Terminal`.
- `WebStream` → `of_boxed`, which downcasts the boxed leaf: a `genai::Error`
  **recurses back into `Fault::of`** (so a boxed `HttpError` reuses the same
  `Status` arm), a `reqwest::Error` goes to `of_reqwest`, and a corrupt-UTF-8
  box falls through to `Terminal`.
- `ChatResponse` → read `json_status_code` from the body; a valid code becomes
  `Status`, a missing one is `Terminal`.
- every other variant → the `_ => Terminal` floor.

`of_reqwest` is the one predicate left: a `reqwest::Error` is `Transport` only
for `is_connect() | is_timeout() | is_request() | is_body() | is_decode()` — the
classes a re-issue can plausibly clear. A builder or redirect fault is the
caller's to fix and stays `Terminal`.

**The `_ => Terminal` floor makes the walk total.** A genai version that adds a
new variant defaults to "surface it," never to a silent retry on a guess —
since genai is [[map/exarch/provider|vendored]], a new transient shape is a
deliberate edit to `Fault::of`, not something a string heuristic catches by
luck. (The walk replaced an earlier `status_of` + `Display`-substring fallback
that both over-matched — the word "body" appears in a non-JSON error's own
`Display` — and under-matched. The string never decides recovery now.)

## From leaf to verdict: `ProviderError`

`from_genai` turns the leaf into the public failure the rest of exarch sees,
`ProviderError`. Only the `Status` leaf needs a decision — the HTTP code splits
three ways:

| leaf | `ProviderError` | retried? |
|---|---|---|
| `Status` 429 | `RateLimited { retry_after, cause, body }` | yes — patient tier |
| `Status` 5xx | `Transient { cause, attempts, body }` | yes — transient tier |
| `Status` other (4xx, redirect) | `Api { status, model, message, url, body }` | **no** — the user must change the request |
| `Transport` | `Transient` (no body) | yes |
| `Terminal` | `Other(String)` | no — rendered raw |

For a 429 the wait is honoured precisely: `retry_after_header` reads the
structured `Retry-After` straight off the response headers when carried,
falling back to `parse_retry_after` scraping the cause text only when it is not.
(`parse_retry_after` slices the *lowercased* copy it searches, so a
length-changing lowercase like `İ` can never land mid-character and panic.)

Every retryable and 4xx variant carries the provider's parsed JSON body as
`Option<Value>` to the boundary, so [[map/exarch/cards|the renderer]] can print
a structured, labelled error rather than scraping cause text. A non-JSON body
(an HTML 5xx page) leaves it `None` and the cause string stands in.

Two more `ProviderError` variants never come from `from_genai`:

- **`Cancelled(&'static str)`** — the user raised the cancel flag in flight; the
  `&'static str` records *where* so the UI pins the blame ([[internals/cancellation|cancellation]]).
- **`Truncated { reason }`** — the assistant turn was cut off *cleanly* by the
  output cap or a stall, raised after the partial assistant message is already
  appended, so re-prompting with `continue` preserves the work.

## The retry driver

Both the streaming and summary paths run through one driver,
`retry_with_backoff`, over an `Attempt<T>`:

```
enum Attempt<T> { Done(T), Failed(ProviderError) }
```

The loop is small and the rules read straight off it:

- A `Done` returns the value. A `Failed(Cancelled)` returns immediately — a
  cancel is never retried or reclassified.
- A `Failed(e)` retries **only** when `e` is `Transient` or `RateLimited` and
  budget remains; any other variant (`Api`, `Other`) is stamped with its final
  attempt count and surfaced. So a 4xx never burns the budget.
- Between attempts it `select!`s the backoff sleep against the cancel token, so
  a user can interrupt a wait.

`retry_limits` gives rate limits a strictly more patient tier than generic
transient faults — they are the only thing now retried on a 429, so they must be
the patient one:

| tier | attempts | delay ceiling |
|---|---|---|
| `Transient` | `MAX_ATTEMPTS` = 3 | `MAX_DELAY_MS` = 8 s |
| `RateLimited` | `RATE_LIMIT_MAX_ATTEMPTS` = 6 | `RATE_LIMIT_MAX_DELAY_MS` = 30 s |

`backoff_sleep` is exponential — `BASE_DELAY_MS` (750 ms) × 2^(attempt−1),
capped at the tier ceiling — but a server's explicit `retry_after` overrides the
curve (itself capped at the ceiling, so a hostile header can't park us forever).

Because transport retry lives entirely here, [[map/exarch/agent|the nudge
rules]] upstream cover only *model-behaviour* outcomes — they never see a
transport blip.

## The streaming-specific rule: commit, don't double-render

Streaming adds one wrinkle the driver respects. Once text or reasoning has
flowed to `on_text` / `on_think`, the UI has *committed* to a partial render; a
re-issue would print that content twice. So `complete` tracks whether either
stream has yielded content, and when the stream breaks:

- **no text or reasoning streamed** → return `Attempt::Failed(Transient)` and
  let the driver re-issue (nothing was shown).
- **content already streamed** → `stalled_step_out` projects the text prefix
  and reasoning into a `CutShort::Stalled` `StepOut` — no tool calls, no stop
  reason, the stall cause carried for the operator note — and hands it back as
  `Attempt::Done`. The session commits the prefix and continues the exchange,
  mirroring the output-cap truncation path.

This is why `Attempt` needs no third "don't retry" variant: a committed stall is
simply a `Done` carrying partial work. A stream that ends without an `End` frame
is classified `Transient` for the same machinery — retried when nothing showed,
committed when something did.

## The idle timeout

Before a streaming response opens, `idle_timeout` bounds connect and
time-to-first-event: the first attempt gets `STREAM_IDLE_TIMEOUT` (180 s), then
retries get `RETRY_IDLE_TIMEOUT` (60 s). Once a stream is open there is no
timeout between decoded `ChatStreamEvent`s. Liveness moves down to the
transport's 180-second per-read timeout, so raw byte silence fails while SSE
pings or provider heartbeats consumed below the semantic event layer keep a
long-thinking model alive. A read timeout surfaces as a stream error and enters
the same transient-or-committed rule above.

The worst pre-stream idle burn is bounded by construction at 180 + 60 + 60 =
300 seconds. A non-streaming summary has no incremental events, so its
per-attempt `idle_timeout` bounds the whole `exec_chat` call. Tests pin the
first-attempt/retry distinction and the aggregate budget; the transport rule is
the local slice of [[decisions/260702_provider-heartbeats-and-retry-boundaries|provider-heartbeats-and-retry-boundaries]].

## What is deliberately terminal

The instinct to retry everything is wrong, and the structural walk makes the
non-retryable cases explicit rather than accidental:

- A **4xx** (`Api`) is the request itself — a bad key, an unknown model, a
  malformed body. The DeepSeek session log once showed a hard 400 wrapped in a
  streaming `WebStream`; retried, it burned the whole budget on an unfixable
  request. It is now `Api`, surfaced at once.
- A **non-JSON 2xx** (`ResponseFailedNotJson`) is a provider contract breach,
  not a blip — `Terminal`.
- A **UTF-8 corruption** boxed in a `WebStream` is `Terminal` — re-reading the
  same bytes won't decode them.
- An **input / auth / mapping** error is the caller's to fix — `Terminal`.

## Testing the classifier without a network

The suite tests two independent axes — the *source* (which genai variant
carried the fault) and the *outcome* (`RateLimited` / `Transient` / `Api` /
`Other`) — and pins each **once**, not their cross product. A handful of
hand-built `genai::Error` fixtures cover every source: a `WebStream` boxing an
`HttpError` (recursion + 4xx, the named 400 regression), a `WebModelCall` with a
`Retry-After` header (429 + the header read), a compaction 5xx, a `ChatResponse`
JSON frame, a non-JSON `WebModelCall` (the contract-breach `Terminal`), and a
`WebStream` with an unrecognised boxed cause (the `Terminal` floor).

One gap is honest and unavoidable: the `Transport` leaf has no direct unit test,
because a `reqwest::Error` cannot be constructed outside a live socket. It rests
on reqwest's own `is_timeout()` / `is_connect()` predicates, and is exercised
transitively where a `Transient` cause drives the stall projection. Closing it
for real would need a network integration test (connect to a dead port) — flaky
and slow — so it stays open rather than faked. The retry driver itself *is*
tested end to end: a forced `Transient` is driven through the real loop and must
exhaust `MAX_ATTEMPTS`, then surface bounded — the same path that stamps the
attempt count.

## See also

- [[map/exarch/provider|provider]] — the transport this classifies for:
  identity, client building, the streaming and summary calls, the idle timeout,
  usage and pricing.
- [[internals/cancellation|cancellation]] — the per-exchange `Token` the
  `Cancelled` variant reports, and the cancel-aware backoff select.
- [[map/exarch/agent|agent]] — the deliberate loop above the provider, where a
  `Truncated` compaction summary and the nudge-retry rules live.
- [[map/exarch/cards|cards]] / [[map/exarch/frontend|frontend]] — the structured
  error chrome that reads the parsed JSON `body`.
- [[design/failure|failure]] — ral's own status-vs-truth failure model, a
  contrast: there failure is a propagating status, here a fault is a recovered
  transport outcome.
- `exarch/src/provider/error.rs` (`from_genai`, `Fault`),
  `exarch/src/provider/retry.rs` (`retry_with_backoff`), and
  `exarch/src/provider/tls.rs` (`STREAM_IDLE_TIMEOUT`, the `read_timeout` it
  backs).
