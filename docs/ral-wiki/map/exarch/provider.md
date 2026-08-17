---
generated_at_commit: cbeb5457
generated_at_date: 2026-08-17
covers_paths: [exarch/src/provider.rs, exarch/src/provider/, exarch/src/tui/model_picker.rs]
---

# Map: exarch / provider

`provider.rs` is the LLM facade over `genai`: it owns `Provider`, its live or
scripted `Backend`, and OpenRouter route admission. The transcript owns
history; the provider only sends bytes and parses replies. Invariants are
local below the facade: `provider/identity.rs` owns selectable identity,
`request.rs` wire shaping (including `Tuning` and the `EFFORT_LADDER` rungs;
the TUI keeps only the glyphs), `transport.rs` credential binding and caching,
`stream.rs` completion and summary execution, `retry.rs` recovery timing,
`usage.rs` accounting, `error.rs` fault classification, and `listing.rs`
model-list fetch orchestration. The public facade
re-exports their established types; sibling modules meet through narrow
methods on `Engine` and `Transport`, not visible fields.

## Services and accounts

Selectable identity is a **product of two things**, not a sum over provenance.
A *`Service`* is where bytes go — endpoint, wire adapter, billing, model list.
An *`Account`* is who is asking — an id, a credential, and a name for itself.
One service may own many accounts: a ChatGPT login email carries a personal
account and one per workspace, and OpenAI issues each its own id against the
same `email` claim. A key-bearing service is the degenerate case — one service
with exactly one account, which borrows the service's name — and that is why
the flattened design never went wrong there.

`provider/identity.rs` owns both, and the two newtypes that keep them apart:

- `ServiceName` has two doors, because its sources differ in trust: the
  built-in table is known good, a declaration is input. `declared` refuses an
  empty, colon-bearing, or control-bearing name, each with its own sentence.
  A declared name equal to a built-in's is refused at load, naming it — once
  an account id is a service name, a shadow is an identity collision rather
  than the silent shadowing it used to be.
- `AccountId` renders a key-bearing account as its service's name, and a login
  as `"{service}:{issued}"`. Service names carry no colon, so the first colon
  separates the halves and the rendering is injective. **Nothing ever parses
  one.** `state.json`, the model cache, the record log, synod's wire and
  `--provider` all compare a rendering against the renderings of the accounts
  actually present; there is no `from_str`, and adding one would reintroduce
  the ambiguity the type exists to prevent.

`built_in_services()` is the table — nine key-bearing services (Anthropic,
OpenAI, OpenRouter, DeepSeek, Gemini, opencode Zen, opencode Go, xAI, Qwen)
plus chatgpt — as struct literals rather than a value enum. A declared
endpoint is the *same struct* parsed from a hand-written `config.ral`, with
the protocol mapped onto genai's `AdapterKind` at decode time; provenance is
not a type. See [[decisions/260613_provider-config-ral-script|provider-config-ral-script]].
`Service::routes` is true for OpenRouter alone, and carries both route pinning
and the `vendor/model` fallback, so no code compares a service name against
the string `"openrouter"`. `Service::auth` says what a *declaration* knows
about the bearer token — `Env(var)`, `OAuth`, or `Unnamed` — and never where
the secret is kept, which is the one thing exarch and synod disagree about.

**Every map keys on an `AccountId`.** Accounts are owned once, in
`CredentialStore::all: Vec<Account>`; the store's `ready`, `admitted` and
`environment` layers, the model catalog's memo and its disk cache, the
listing's states and in-flight fetches, the transport cache's key, and the
picker's model states are all keyed by id. `available()` filters `all` by
membership in `ready`, so no view holds a copy that can go stale.

### A handle is local; a label is set-relative

An `Account::handle` is derived from that account's own credential alone: the
`id_token`'s email claim, else the issued account id, qualified by the
workspace title or the plan type when the token carries one. For a key-bearing
service it is simply the service's name. Being local is the point — admitting
one account is complete and correct on its own, so `add_oauth` needs no set,
and a refresh that rewrites claims into the token cell leaves nothing stale.

`identity::label(account, among)` computes the **set-relative** name, and is
the one place in either product that names an account. It takes the set
because the answer depends on it, and it is never stored: a name that is a
function of the whole set cannot be cached in a single record, because there
is then no reconciliation path when a sibling arrives or leaves. The service
alone when the handle is the service's name, the service and the handle both
otherwise, and the id to separate a tie — nothing is decorated, because how an
account bills is `Billing`'s business, not its name's. The tie check also compares against the
other accounts' id-qualified forms, so a handle that happens to spell one out
(the claims are issuer- and workspace-supplied) cannot impersonate a sibling's
label.

```
anthropic
opencode-go
chatgpt · alex@bristol.ac.uk
chatgpt · alex@work (Acme Ltd)
```

The honest limit: a live session's status line keeps the handle it started
with, because a mid-session refresh does not re-derive it. A renamed workspace
appears at the next launch.

The one place the log may hold a label is `record.jsonl`, where it is a
*snapshot* of what the account was called when the session began — which is
what a log is for.

### One authority on metering

`Service::billing` is the sole authority on whether a turn costs money, and
`Transport::metered` is `billing == Billing::Metered` — the whole derivation.
chatgpt and opencode Go are both `FlatRate`; a subscription turn reports
tokens and never a cost. The fact was previously derivable two ways that could
disagree, and the second derivation would have reported a future *metered*
OAuth service as free.

On disk the login store is persisted through one door, `write_private`
(`provider/secret_file.rs`), and the file is *born* owner-private: the Unix arm
opens it mode `0600`; the Windows arm passes an owner-only,
inheritance-protected DACL in the `SECURITY_ATTRIBUTES` of `CreateFileW`
itself, so at no instant does the token file wear the parent directory's
inherited ACL. The document is an object keyed by the rendering of an
`AccountId`, one entry per account; an entry whose key disagrees with its own
fields is dropped with a warning rather than trusted, the key being an index
and the fields the truth.

## Two sources for a key, and one door to each

Environment resolution is exarch's own story: `CredentialStore::resolve_and_scrub`
sweeps the conventional key variables once, at single-threaded startup, and
scrubs what it found so no child of a tool call inherits a live key. The
sweep is therefore un-repeatable by construction, which is why every
mid-session admission has its own door.

The **other** source is the computer's own credential manager, and it exists
for [[map/synod|synod]]'s sake — **exarch calls none of it**:

- `provider/keychain.rs` — `Keychain::for_app(App)` reaches the macOS
  Keychain, the Windows Credential Manager, or a Linux desktop's Secret
  Service through one `keyring` entry named `(app, account-id)`, so two
  products' keys are two entries exactly as their config directories are two
  directories. Only key-bearing accounts reach the vault, and their ids *are*
  their service names, so an entry name stays short printable ASCII and no
  `chatgpt:<issued>` ever becomes a Credential Manager target.
  `Entry::store_status()` is asked first; a computer with no
  credential manager falls back to an owner-only file beside the app's
  configuration, and `vault()` answers where secrets actually land in a
  sentence a window prints verbatim rather than implying a protection that is
  not there. A blank or control-bearing entry reads as *no key*, the rule
  `credential.rs` already applies to a key read from the environment.
- `provider/secret_file.rs` — `write_private`, the owner-only writer the
  `ChatGPT` token store and that fallback file share: one implementation of
  the `0600`-at-`open` and owner-only-DACL-at-`CreateFileW` promise instead of
  two copies.

`credential.rs` carries four mid-session mutators that exist for a *window*,
not for exarch: `known` (every account, bound or not — the list an accounts
screen is drawn from, since one with no key is precisely the one a user has
come to give a key to), `admit_key` (bind a key now, `add_oauth`'s sibling for
the un-repeatable sweep), `forget` (unbind, leaving the account known), and
`retire` (drop it entirely — what withdrawing a declaration means). The store
also remembers which door each key came through (`was_admitted`), because only
it knows, and an application re-deriving that by interrogating its vault would
pay a round trip per account every time it drew a list. See
[[decisions/260807_synod-keeps-its-own-accounts|synod-keeps-its-own-accounts]].

The vault itself reaches the store through one seam: `SecretVault::read` by
account, and `CredentialStore::admit_from`, an ordinary second call after the
sweep that lays the vault over the top — so a key typed into an accounts screen
outranks a stale environment variable. It scrubs nothing, which is why
`resolve_and_scrub`'s single-threaded contract is untouched by its existence.
`provider/accounts.rs` holds the rest of what a window needs and exarch knows:
`declared_endpoints`, `declare_endpoint`, `withdraw_endpoint`, `checked_key`,
and a `find` that resolves by `AccountId` alone.

`config.rs` is likewise generalised without changing exarch's path: `load()`
is now `load_declared(path, label)` over exarch's own file, and `save_declared`
writes the same `.ral` source back — a file a program wrote and a person can
still edit, which is the whole reason declarations are `.ral` and not an
opaque blob. A label or address carrying a quote is refused with a question
rather than escaped.

## Building the transport

`provider/transport.rs::build_client` binds the resolved `Credential` to a
genai `Client`:

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
  `refresh_cell_if_stale` is the common renewal door for inference and catalog
  requests, upserting just that account's entry. A refresh may rename the
  account — fresh claims update the handle's ingredients — but never re-keys
  it: the issued id is pinned to the current token's, since every map keys on
  it.

## Model catalogs

**The picker asks each provider for its own names and retains manual entry as
the total fallback.** `ModelCatalog` memoises and disk-caches both paths:

- API-key providers list through genai's `all_model_names`.
- ChatGPT accounts list through `/backend-api/codex/models`, authenticated by
  their live OAuth cell after the common stale-token check.
- `/login` admits an account mid-session through
  `CredentialStore::add_oauth`; that operation returns the id and the exact
  shared `Credential`, which `ModelCatalog::add_credential` admits through
  its narrow live-source seam. Re-login updates the cell in place; a login
  that has since learnt its email is renamed where it stands, its identity
  being the account id throughout.
- OpenRouter serving endpoints remain a separate, intent-driven request after
  a model is selected.
- `listing.rs` states the picker-side orchestration once for every front-end
  (the `/model` overlay and synod's window alike): the `FetchState` vocabulary,
  a keyed background-fetch pump (`Fetches`), and the per-provider `Listing`
  that seeds from the catalog's cache and fills misses in as they land.

## The streaming and summary paths

- `complete(system, messages, tool_enabled, search, on_delta, cancel)` —
  streams one assistant reply, calling `on_delta` with a `Delta::Say` per text
  chunk and a `Delta::Think` per reasoning chunk, and projects the `StreamEnd`
  into a `StepOut` (assistant message, tool calls, `Usage`, `StopReason`). One
  callback rather than two because the *order* of the two kinds is the
  caller's whole interest: a reasoning run ends exactly where the prose after
  it begins, and two independent callbacks cannot say so. It preserves
  `reasoning_content` on the assistant message so thinking mode round-trips —
  that is the only route reasoning takes back into context, since the display
  commit is authored from the deltas.
  The stream-event match is exhaustive — thought-signature and tool-call chunks
  are captured in the `End` frame and replayed by `step_out_from_end`, so they
  are dropped by a *named* arm, never a wildcard, and a new genai stream variant
  fails the build (X10).
- A **per-attempt idle timeout** bounds request open and every gap between
  decoded `ChatStreamEvent`s. The first attempt uses `STREAM_IDLE_TIMEOUT`
  (180s, `provider/tls.rs`); retries use a one-minute bound. Each event,
  including a provider heartbeat, re-arms the semantic watchdog, while
  reqwest's per-read timeout turns true byte-level silence into a retryable
  stream error. A ping genai consumes below the semantic event layer may keep
  the transport read alive, but cannot re-arm exarch's event watchdog; only a
  decoded event can do that.
  This lands the first local slice of
  [[decisions/260702_provider-heartbeats-and-retry-boundaries|provider-heartbeats-and-retry-boundaries]].
- `summarize` — one non-streamed call producing a compaction summary; used by
  [[map/exarch/agent|`Agent::compact`]]. The same idle timeout bounds the
  whole `exec_chat` request (no incremental events to idle between). A summary
  that itself hit the 1024-token budget is surfaced as `Truncated`, so
  `compact` keeps the un-summarised history rather than committing a half
  summary (X10).
- `cancel` is the **request-local** cancellation handle: the foreground exchange
  passes its root token (Esc-linked), an async `agent` passes its registry
  token, so two concurrent requests no longer share one process-global slot —
  the provider-side seam of [[decisions/260617_async-agent-tool|async-agent-tool]];
  the registry and inbox belong to [[map/exarch/tools|tools]] and
  [[map/exarch/agent|agent]].

## Retry driver

Both paths in `provider/stream.rs` run on a tokio runtime through **one retry
driver**, `provider/retry.rs::retry_with_backoff`, over an `Attempt<T>` (`Done`
/ `Failed`):

- The streaming-specific rule rides on `Done`: once text or reasoning has
  flowed to `on_delta` the UI has committed to a partial render, so a
  re-issue that would double output is *not* retried — `stalled_step_out`
  projects the streamed prefix and reasoning into a `CutShort::Stalled`
  `StepOut` returned as `Attempt::Done`, and the session commits it. No third
  "don't retry" variant is needed.
- Rate limits get a larger budget and a higher backoff ceiling than transient
  failures (`retry_limits`), and an explicit `retry-after` is honoured.

Transport retry lives here, so the [[map/exarch/agent|nudge]] rules cover
only model-behaviour outcomes, not transport.

## Structural error classification

`ProviderError::from_genai` (`provider/error.rs`) **reads retryability from
genai's typed variants, not from its `Display` string.** A single structural walk, `Fault::of`, descends
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

Each retryable and 4xx variant carries the parsed body (boxed at the error
boundary) as an optional JSON value, so the renderer can print a
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
- `provider/pricing.rs` fetches OpenRouter's `/api/v1/models` catalog once per process
  and caches it; `ModelPricing::dollars` strips the cache counts out of `input`
  and bills uncached/cache-creation/cache-read/output each at its rate, falling
  back to the base input rate when no separate cache rate is published. Native
  DeepSeek models use the local rate table (including its UTC peak windows)
  before any OpenRouter alias; other providers use the catalog. The catalog
  also supplies `ModelCaps` (context window and supported request parameters)
  for startup and picker decisions. Offline starts degrade to `—`.

`provider/request.rs::build_cached_request` sets two `cache_control: ephemeral`
breakpoints (system + tools, growing transcript) for the message-based adapters,
and a per-process
`prompt_cache_key` for OpenAI shard routing. The Responses adapter is the
exception: its system prompt rides the top-level `instructions` field, since a
`System` message would leave `instructions` empty and the Codex backend rejects
that. `tool_defs(adapter, tool_enabled, search)` builds the request's tool array:
the `ral` wire tool under `tool_enabled`, plus the provider's own hosted
web-search tool under `search` — carried only on the three adapters genai maps it
for (`OpenAIResp`, `Anthropic`, `Gemini`), and `OpenAIResp` alone adds the
`external_web_access` config that switches codex from its cached index to the
live internet; the bit is [[map/exarch/agent|agent]]'s `search`.
