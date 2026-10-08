---
verified_at_commit: 6a8d848f
verified_at_date: 2026-10-08
anchors: [from_genai, refused, error_object, Fault, of_webc, of_boxed, of_reqwest, ProviderError, Refused, Refusal, Limit, Recovery, recovery, Transient, Api, Truncated, retry_with_backoff, Attempt, backoff_sleep, parse_retry_after, unwaitable, BODY_KEYS, epoch_or_delta, Wait::patient, RATE_LIMIT_MAX_DELAY_MS, json_status_code, CutShort, stall_cause, root_cause, body_detail, Readout, stalled_step_out, STREAM_IDLE_TIMEOUT, MAX_ATTEMPTS, RATE_LIMIT_MAX_ATTEMPTS, manufacture, Sealed]
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

The transport itself — identity, client building, the streaming call — is
[[map/exarch/provider|the provider map]]; this is the failure half.
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
| `WebModelCall` / `WebAdapterCall { webc_error }` | non-streamed calls: adapter model-list fetches | a `webc::Error` (below) |
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
    Transport(Option<String>),
    Terminal,
}
```

- **`Status`** — a completed HTTP response reached us with a non-2xx code. The
  code, the headers (for `Retry-After`), and the provider's JSON error body
  ride along; the *caller* decides what a given status means.
- **`Transport`** — a `reqwest` fault with no status: connect, timeout, a
  malformed request, or a body that dropped or failed to decode mid-flight.
  Retryable by nature — nothing reached the model. It carries the leaf's own
  `source` chain, the only thing that tells one of these apart.
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
and since `exarch/Cargo.toml` names one exact genai prerelease, a new transient
shape arrives only with a deliberate bump and a deliberate edit to `Fault::of`,
never caught by luck from a string heuristic. (The walk replaced an earlier
`status_of` + `Display`-substring fallback
that both over-matched — the word "body" appears in a non-JSON error's own
`Display` — and under-matched. The string never decides recovery now.)

## From leaf to verdict: `ProviderError`

`from_genai` turns the leaf into the public failure the rest of exarch sees,
`ProviderError`. Only the `Status` leaf needs a decision — the HTTP code splits
three ways:

| leaf | `ProviderError` | retried? |
|---|---|---|
| `Status` 429, quota spent | `Api` | **no** — no wait clears it |
| `Status` 429, otherwise | `Refused(Refusal { limit, resets_at, received, cause, body })` | as `Refusal::recovery` reads it: in place (patient tier), or surfaced at once |
| `Status` 5xx | `Transient { cause, attempts, body }` | yes — transient tier |
| `Status` other (4xx, redirect) | `Api { status, model, message, body }` | **no** — the user must change the request |
| `Transport(detail)` | `Transient` (no body) | yes |
| `Terminal` | `Other(String)` | no — rendered raw |

A 429 is read by `refused`. A body whose error `type` or `code` names a quota
or credit spent (`insufficient_quota`, `usage_not_included`, the spend- and
usage-limit codes, and `subscription_sharing_usage_limit_exceeded`, a
sign-in plan's cap, whose reset the route does not name) is `Api`: waiting
does not refill a purse. Every other 429 is stated whole as a
`Refusal`: what ran out (`Limit`: `Allowance` when the body's `type` or `code`
is `usage_limit_reached`, else `Rate`), when it was `received`, and the reset,
which `provider/reset.rs`'s `at` reads from every convention one is named by,
taking the **latest** instant any names — asking before the last named clock
runs out is only refused again:

| where | convention | who |
|---|---|---|
| `retry-after-ms` header | delta milliseconds | OpenAI SDK convention, opencode |
| `retry-after` header | delta seconds, or an HTTP-date (jiff's RFC 2822 parser) | RFC 9110 |
| `x-ratelimit-reset`, `ratelimit-reset` headers | `epoch_or_delta` | GitHub-style, IETF draft |
| body `resets_at`, `resets_in_seconds`, `retry_after_seconds`, `retry_after` | `epoch_or_delta` | a `usage_limit_reached` body (an epoch), others (a delta) |
| body `details[]` `google.rpc.RetryInfo` → `retryDelay` | `"38s"` | Gemini |
| body `metadata.headers` `x-ratelimit-reset` | `epoch_or_delta` | OpenRouter's relayed limit |

`epoch_or_delta` is the one convention that varies by vendor — epoch
milliseconds, epoch seconds, or a delta — and magnitude alone tells them apart,
since no real wait is 31 years long. Only when no structural reader answers
does `parse_retry_after` scrape the cause text. (It slices the *lowercased*
copy it searches, so a length-changing lowercase like `İ` can never land
mid-character and panic.)

How to recover is not part of a `Refusal` but the retry policy's reading of
it, `Refusal::recovery`: `InPlace(Some(Wait))` when the wait from `received`
is within `RATE_LIMIT_MAX_DELAY_MS` (only `Wait::patient` makes a `Wait`),
`InPlace(None)` when none is named, and `Deferred(at)` past the ceiling. The
loop, the hold, the resume and the renderers all share this one reading; the
record carries the `Refusal` itself, so a replay reads it as the live run did.
`record::fault`'s readout suppresses `reset::BODY_KEYS` from the body dump,
since the dedicated field already carries them.

A transport leaf carries its deepest `source` (`root_cause`) as `detail`, and
that becomes the cause outright — genai's wrapper text above it is discarded,
as it already is for `Terminal`. Without the leaf the whole class is mute:
reqwest maps *every* mid-stream body failure — a reset peer, an h2 `GOAWAY`, a
truncated chunk, this client's own read timeout — through the single `Display`
string "error decoding response body", and only the root beneath it says which
happened. The intermediate links are as generic as the wrapper, so the reader
is shown the one line that names the fault.

Every retryable and 4xx variant carries the provider's parsed JSON body as
`Option<Value>` to the boundary, so [[map/exarch/cards|the renderer]] can print
a structured, labelled error rather than scraping cause text. A non-JSON body
(an HTML 5xx page) leaves it `None` and the cause string stands in.

A failure has exactly one reading at each of its two widths. The flat one is
`summary()`, which `Display` now delegates to outright: a failure crossing an
agent boundary, printed by an error chain, or interpolated by a caller who
reaches for `{e}` all get the same sentence, and genai's multi-line
`Cause:`/`Status:`/`Body:` framing reaches none of them. The structured one is
`record::fault::Readout` — a headline and ordered `(label, value)` fields,
neutral about presentation — which the [[map/exarch/cards|TUI block]] styles
and the headless printer writes as `  label: value` lines. Neither surface
describes a failure itself, so neither can disagree with the other about what
one is.

Where a raw response body must still appear in a message — a sign-in failure,
a model listing — it passes through `body_detail`, which prefers the body's own
JSON message, names an HTML error page rather than spilling it, and caps every
shape alike: a proxy's page can reflect request context, and the string reaches
both the screen and the log.

Two more `ProviderError` variants never come from `from_genai`:

- **`Cancelled(&'static str)`** — the user raised the cancel flag in flight; the
  `&'static str` records *where* so the UI pins the blame ([[internals/cancellation|cancellation]]).
- **`Truncated { cause: CutShort }`** — the assistant turn was cut off *cleanly*
  by the output cap or a stall, raised after the partial assistant message is
  already appended, so re-prompting with `continue` preserves the work. The cut
  rides on as `CutShort` — `OutputCap { stop_reason }` or `Stalled(cause)` —
  rather than as one flattened reason string, because the two have different
  remedies: a cap is a ceiling the user raises with `--max-tokens`, a stall is
  nobody's to fix and is survived. A single string made the renderer print the
  cap's remedy under a dropped connection.

  That is also the whole record of the cut. The stall once wrote a second
  record of its own at the point it happened, so a survived stall drew two
  error blocks — one saying the turn resumes, one immediately reading as the
  end of the run. Now the caller's `record_provider_error` writes it once, and
  `ProviderError::stall_cause` is the one place the "survived, not fatal"
  reading is derived: the TUI fold, synod's seam and the headless printer each
  ask it rather than re-matching the shape.

## The retry driver

The streaming path runs through one driver, `retry_with_backoff`, over an
`Attempt<T>`:

```
enum Attempt<T> { Done(T), Failed(ProviderError) }
```

The loop is small and the rules read straight off it:

- A `Done` returns the value. A `Failed(Cancelled)` returns immediately — a
  cancel is never retried or reclassified.
- A `Failed(e)` retries **only** when `e` is `Transient` or a `Refused` whose
  `Refusal::recovery` is `InPlace`, and budget remains; any other variant (`Api`, a
  `Deferred` `Refused`, `Other`) surfaces at once, and a `Transient` out of
  budget is stamped with its final attempt count. So a 4xx never burns the
  budget, and a refusal past the patient tier surfaces on its first attempt.
- Between attempts it `select!`s the backoff sleep against the cancel token, so
  a user can interrupt a wait.

Each retry re-**manufactures** the request rather than cloning one.
`Engine::complete` (`provider/stream.rs`) holds
`&Transcript` — the shared, `Arc`-backed history — and calls
`provider/wire.rs::manufacture` inside the retry closure, once per attempt.
There is no request template kept alive across attempts to clone: `manufacture`
returns a `Sealed(ChatRequest)` that is deliberately not `Clone`, so a second
copy of a built request cannot be expressed on this path even by accident. A
retry storm still pays one whole-history deep copy per attempt — the genai
floor, since `exec_chat_stream`/`exec_chat` consume an owned request — but it
is bounded by exactly the tiers above, and it is the *only* copy the retry
loop pays, not a copy of a copy. See
[[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]].

`retry_with_backoff`'s one match gives a refusal waited in place a strictly
more patient tier than generic transient faults — it is the only thing retried
on a 429, so it must be the patient one:

| tier | attempts | delay ceiling |
|---|---|---|
| `Transient` | `MAX_ATTEMPTS` = 3 | `MAX_DELAY_MS` = 8 s |
| `Refused`, in place | `RATE_LIMIT_MAX_ATTEMPTS` = 6 | `RATE_LIMIT_MAX_DELAY_MS` = 30 s |

`backoff_sleep` is exponential — `BASE_DELAY_MS` (750 ms) × 2^(attempt−1),
capped at the tier ceiling — but a server's explicit `Wait` overrides the
curve, uncapped: a `Wait` past the ceiling has no spelling, so the ceiling is
also the line between a refusal waited out here and one deferred to the agent.

## Resuming at a refusal's reset

A `Deferred` refusal ends the turn honestly, like every surfaced provider
error — the transcript does not grow
([[decisions/260702_provider-heartbeats-and-retry-boundaries|provider-heartbeats-and-retry-boundaries]]).
Only exarch's terminal trunk then arms a wakeup of its own
(`resume_on_reset`): `resume_at` replaces any `provider-reset` schedule with a
one-shot at the reset, whose prompt says when requests were refused and until
when, and to carry on. It is a wall-clock `Trigger::At`, and fires as an
ordinary `Post::Wakeup`, a new exchange the log admits after
`quiesce(Aborted)`. The harness arms it, not the model, so the
`allow_schedule` grant does not apply. Nobody else resumes: a headless run
fails fast with the reset in its summary (`usage limit reached until Tue
14:05`), synod's exchange ends on the refusal (Law B leaves nothing to wake
it), and a child fails up to its parent, whose next request meets the hold
itself. Every fork gets `resume_on_reset: false`, and `converse_settled`
refuses a trunk that holds it.

The refusal is also the account's or the model's, not only the turn's:
`Rations::settle` keeps its `resets_at` in the per-account record
([[map/exarch/provider|provider]]), scoped by what it says ran out — the whole
account for an `Allowance`, one model for a `Rate`. `Provider::complete`
answers a request with the same `Refused` before anything is sent, but
**only** when the hold's `Refusal::recovery` is `Deferred`; a nearer hold is
let through, and the provider's own refusal is waited out in place. A sibling
agent, a `/model` switch back, or a user's typed prompt all meet the hold
rather than a doomed request. One limit stands: schedules live in memory only.

Because transport retry lives entirely here, [[map/exarch/agent|the nudge
rules]] upstream cover only *model-behaviour* outcomes — they never see a
transport blip.

## The streaming-specific rule: commit, don't double-render

Streaming adds one wrinkle the driver respects. Once text or reasoning has
flowed to `on_delta`, the UI has *committed* to a partial render; a
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
transport's per-read timeout, so raw byte silence fails while SSE pings or
provider heartbeats consumed below the semantic event layer keep a
long-thinking model alive. A read timeout surfaces as a stream error and enters
the same transient-or-committed rule above.

That read timeout is held 30 s clear of `STREAM_IDLE_TIMEOUT` (210 s) so the
two bounds do not race: whichever fires names the failure, and only the stream
loop's own arm can say *which* wait ran out. Held equal, reqwest's — armed on
the last byte, not the last event — won every first attempt, and every silent
provider read as "error decoding response body". The socket bound is the
backstop under the semantic one, for bytes stopping where no `next()` waits.

The worst pre-stream idle burn is bounded by construction at 180 + 60 + 60 =
300 seconds. Tests pin the
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
- A **429 naming a spent quota** (`insufficient_quota` and kin) is `Api`:
  neither a retry nor a wakeup can clear it.

## Testing the classifier without a network

The suite tests two independent axes — the *source* (which genai variant
carried the fault) and the *outcome* (`Refused` / `Transient` / `Api` /
`Other`) — and pins each **once**, not their cross product. A handful of
hand-built `genai::Error` fixtures cover every source: a `WebStream` boxing an
`HttpError` (recursion + 4xx, the named 400 regression), a `WebModelCall` with a
`Retry-After` header (429 + the header read), a
`usage_limit_reached` body (`Refused` as an `Allowance`, deferred to its instant), an
`insufficient_quota` body and a `subscription_sharing_usage_limit_exceeded` one (each `Api`), a non-streamed 5xx, a `ChatResponse`
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
  identity, client building, the streaming call, the idle timeout,
  usage and pricing.
- [[internals/cancellation|cancellation]] — the per-exchange `Token` the
  `Cancelled` variant reports, and the cancel-aware backoff select.
- [[map/exarch/agent|agent]] — the deliberate loop above the provider, where a
  `Truncated` reply and the nudge-retry rules live.
- [[map/exarch/cards|cards]] / [[map/exarch/frontend|frontend]] — the structured
  error chrome that reads the parsed JSON `body`.
- [[design/failure|failure]] — ral's own status-vs-truth failure model, a
  contrast: there failure is a propagating status, here a fault is a recovered
  transport outcome.
- `exarch/src/provider/error.rs` (`from_genai`, `refused`, `Fault`),
  `exarch/src/provider/reset.rs` (`at`, `unwaitable`; one test per convention),
  `exarch/src/provider/retry.rs` (`retry_with_backoff`), and
  `exarch/src/provider/tls.rs` (`STREAM_IDLE_TIMEOUT`, the `READ_TIMEOUT`
  backstop under it).
