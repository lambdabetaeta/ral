---
generated_at_commit: dd8a5443b8f87dc40de66bcf5c5a72df05a723d8
generated_at_date: 2026-10-25
covers_paths: [exarch/src/provider.rs, exarch/src/provider/, exarch/src/tui/model_picker.rs, exarch/src/tui/login.rs, exarch/src/tui/providers.rs, exarch/src/tui/commands.rs]
---

# Map: exarch / provider

`provider.rs` is the LLM facade over `genai`: it owns `Provider` and its live
or scripted `Backend`. The transcript owns
history; the provider only sends bytes and parses replies. Invariants are
local below the facade: `provider/identity.rs` owns selectable identity,
`request.rs` per-request `ChatOptions` and tuning (`Tuning`, the
`EFFORT_LADDER` rungs — the TUI keeps only the glyphs), `wire.rs` the one place
that turns the transcript's `ChatMessage`s into an owned `genai::ChatRequest` (below),
`transport.rs` credential binding and caching, `stream.rs` completion
execution, `retry.rs` recovery timing, `usage.rs` accounting,
`error.rs` fault classification, `listing.rs` model-list fetch
orchestration, `tools.rs` the tools a request may advertise (`ToolSpec`,
`Toolset`, wire definitions only; dispatch is `agent::tools`'), `via.rs` the
upstreams a routing service serves a model via (`Via`), and `state.rs` the
persisted choice. The public facade
re-exports their established types; sibling modules meet through narrow
methods on `Engine` and `Transport`, not visible fields.

## Services and accounts

Selectable identity is a **product of two things**, not a sum over provenance.
A *`Service`* is where bytes go — endpoint, wire adapter, billing, model list.
An *`Account`* is who is asking — an id, a credential, and a name for itself.
One service may own many accounts. Chatgpt and Grok distinguish returned
subjects, not emails or assumed workspaces. Codex distinguishes subject and
backend account/workspace id together; two people in one workspace cannot
overwrite one another. A key-bearing service owns exactly one account and
borrows the service's name.

`provider/identity.rs` owns both, and the two newtypes that keep them apart:

- `ServiceName` has two constructors, because its sources differ in trust: the
  built-in table is known good, a declaration is input. `declared` refuses an
  empty, colon-bearing, or control-bearing name, each with its own sentence.
  A declared name equal to a built-in's is refused at load, naming it: an
  account id is a service name, so a shadow would be an identity collision,
  not a harmless override.
- `AccountId` renders a key-bearing account as its service's name, and a login
  as `"{service}:{issued}"`. Service names carry no colon, so the first colon
  separates the halves and the rendering is injective. **Nothing ever parses
  one.** `state.json`, the model cache, the record log, synod's wire and
  `--provider` all compare a rendering against the renderings of the accounts
  actually present; there is no `from_str`, and adding one would reintroduce
  the ambiguity the type exists to prevent.

`built_in_services()` is the table — nine key-bearing services (Anthropic,
OpenAI, OpenRouter, DeepSeek, Gemini, opencode Zen, opencode Go, xAI, Qwen)
plus `Login::ALL`'s `openai-chatgpt`, `openai-codex` and `xai-grok` — as struct literals rather than a value enum. A declared
endpoint is the *same struct* parsed from a hand-written `config.ral`, with
the protocol mapped onto genai's `AdapterKind` at decode time; provenance is
not a type. See [[decisions/260613_provider-config-ral-script|provider-config-ral-script]].
`Service::routes` is true for OpenRouter alone, and is what admits a `Via`, so
no code compares a service name against the string `"openrouter"`.
`Service::auth` says how a service authenticates — `Key(var)`, `OAuth(Login)`,
or `Keyless` — and never the secret itself. A declared endpoint's `var` is
`identity::key_var`'s `{NAME}_API_KEY`, never written in the declaration;
`Keyless` is the declaration's `` key: `none ``, a fact about the server.

**Every map keys on an `AccountId`.** Accounts are owned once, in
`CredentialStore::all: Vec<Account>`; the store's `environment`, `vault` and
`logins` layers, the model catalog's memo and its disk cache, the listing's
states and in-flight fetches, the transport cache's key, and the picker's
model states are all keyed by id. The credential in force is derived from the
layers, never stored beside them: a key is the environment's, else the
vault's; a login is its token cell; a keyless endpoint is the inert bearer.
`available()` is every account with one, so no view holds a copy that can go
stale.

### A handle is local; a label is set-relative

An `Account::handle` is derived from that account's own credential alone: the
email, else the issued identity; Codex qualifies it with workspace or plan.
For a key-bearing service it is simply the service's name. Admission needs
no set; refresh updates only that credential cell. `exarch logout` removes
the local record and attempts issuer revocation for Chatgpt/Grok. Codex
returns `Revocation::LocalOnly`; its refresh credential may remain usable.
Issuer acceptance does not promise immediate access-token invalidation.

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
openai-chatgpt · alex@bristol.ac.uk
openai-codex · alex@bristol.ac.uk (workspace) · <issued id>
```

The honest limit: a live session's status line keeps the handle it started
with, because a mid-session refresh does not re-derive it. A changed email
appears at the next launch.

The one place the log may hold a label is `record.jsonl`, where it is a
*snapshot* of what the account was called when the session began — which is
what a log is for.

### One authority on metering

`Service::billing` is the sole authority on whether a turn costs money, and
`Transport::metered` is `billing == Billing::Metered` — the whole derivation.
The three login services and opencode Go are `FlatRate`; a subscription turn reports
tokens and never a cost. There is no second derivation to disagree with it;
one keyed on the login kind would report a future *metered* OAuth service as
free.

`Service::meter` answers a different question, and is deliberately not derived
from `billing`. `Billing` says *does this turn cost money*; `Meter` says *does
this account publish what is left*. The two are orthogonal and the table says
both: OpenRouter is `Metered` and publishes a credit balance, a declared local
endpoint is `Metered` and publishes nothing. `openai-chatgpt` and `xai-grok`
are `FlatRate` with `Meter::Unpublished`; ChatGPT token-sharing usage is in
Settings → Usage. `openai-codex` is `FlatRate` with `Meter::Codex`, reading
`wham/usage` windows and durations from the server (five hours and one week
are common, not fixed semantics). OpenRouter publishes a credit balance.
Every service states its meter explicitly; billing never implies a readout.

What a meter reports is one value type, `provider/allowance.rs`'s `Allowance`:
a quantity of entitlement, consumed against a bound, renewing over a span. Its
window is a `Duration` and never a vendor's name for one, so the label is
derived here and no provider can inject a display string into exarch's chrome;
its `Consumption` keeps a disclosed proportion and a disclosed count apart,
because flattening them would either discard a purse's only figures or invent a
denominator a provider never stated. Normalisation happens once, at render.

Reading one mirrors `ModelSource` deliberately, so a reader who knows the model
path knows this one: `MeterSource` is the single seam the network sits behind,
`LiveMeters` its live implementation over a `Roster`, and `Survey` the
`Fetches` pump that reads every account concurrently — the same pump `Listing`
uses, unchanged. The one `match` from `Meter` to a request lives in
`LiveMeters::read`, and it is the whole extension point: another vendor is one
`Meter` variant, one arm, one `allowance::meters` module, and one `meter`
argument to the `keyed` builder. Nothing else moves. `/limits` keeps no cache
and no TTL: a ration is an instantaneous fact about a rolling window, and a
stale figure would be wrong in exactly the situation that prompted the
question.

**What is known of each account's allowance has one home, `Rations`
(`provider/rations.rs`), fed on the one road every model request travels.**
The record is the third of the `Holdings`, beside the store and catalog —
exarch builds one, synod one for all its conversations — and keeps, per
account, the last meter reading, the user's ration latches, and the instants
refusals said it is held until: the whole account for a spent allowance, one
model for a rate. `Provider::complete` is the only caller of
`Engine::complete`, so its live arm is where nothing can go around:
`Rations::admit` before the send refuses a held request with `Refused` and
nothing sent — only when the hold's `Refusal::recovery` is `Deferred`, a
nearer one being waited out in place — else spawns a read of the account's
meter when two minutes have passed, its reading landing straight in the
record; `Rations::settle` after it keeps a refusal's `resets_at`, the later of
old and new, on the scope its `Limit` names. The read source rides the
provider: `Bureau::build` clones the roster out under one store lock, binds
the transport to its credential, and hands the roster to a `LiveMeters` in the
same `Backend::Live`, so the meter is exactly as fresh as the transport. The
latches are one per window, so a usage rung is told to the user once per
account, by whichever agent climbs it first (`Rations::climb`). `Rations`
holds no store and takes no lock but its own leaf one. `/limits`
(`Bureau::survey_allowances`) reads every account fresh and `Survey::settle`
lands each reading in the record before drawing its card, so no frontend can
survey without feeding it; the agent's ration gauge
([[map/exarch/agent|agent]]) only climbs `Provider::allowances`, the record's
last reading, at `Allowance::percent` — the figure the card shows, so a
warning and the card never disagree. Success responses carry usage too, but genai does not expose
their headers; when it does, they are one more input to `settle`.

On disk the login store is persisted through `OwnDir::store` with
`Visibility::Secret` (`provider/oauth/mod.rs`): the file is replaced whole and
atomically, and *born* owner-private whatever was there before. The Unix arm
creates it mode `0600`; the Windows arm gives it an owner-only,
inheritance-protected DACL at creation, so at no instant does the token file wear
the parent directory's inherited ACL. A store that cannot be read is never
written over: a failed read aborts the write and returns its error. The document
holds the host id and service-qualified `AccountId` entries. `StoredToken`
stays flat; conversion checks grant-specific required fields and key agreement.
Unknown services (including old `chatgpt`), invalid semantic fields and key
mismatches drop only that entry with a non-secret warning. Invalid JSON or
structurally undecodable required fields make the whole store unreadable.
Queries do not rewrite it; successful writes omit rejected entries. Upserts
compare the full account id, never bare issued identity across services.

### The sign-in

`Login::{Chatgpt, Codex, Grok}` determines service and supported `Method`:
Chatgpt browser, Codex browser/device, Grok device. `SignIn::New(login, method)`
adds an account; `SignIn::Renew(token, method)` refuses a different identity.
`OAuthToken` has common credentials and `Grant`-specific data. Chatgpt/Grok
use nonempty subject as issued identity; Codex uses the injective
`base64url(sub):base64url(chatgpt_account_id)` composite, retaining the raw
backend id separately for headers. Naming metadata does not participate in keys.

Chatgpt preserves dynamic registration, host id, issued client, nonce, scope
and ID-token-hint behavior. Codex reuses the Codex CLI public client and
Grok pi's xAI client; reuse is not issuer approval. Grok's initial ID-token
subject and bearer compatibility with Xai inference **and** `/v1/models` remain
live-unverified. See [[decisions/261008_sign-in-with-chatgpt-token-sharing|sign-in-with-chatgpt-token-sharing]]
for policy and deferred acceptance, including no migration of `chatgpt:*`.

The `/login` overlay chooses accounts or one new-account row per login, then
only supported methods. `LoginPhase` distinguishes browser URL, device
code/URL/expiry and code exchange. Choosing bounds its account viewport;
running/failed phases collapse the roster so instructions remain visible.
Esc closes immediately and signals cancellation; sliced waits observe it
within a tick, while in-flight HTTP remains bounded by its timeout.

## Two layers for a key, the environment's on top

Environment resolution is exarch's own story: `CredentialStore::resolve_and_scrub`
sweeps every `Auth::Key` service's variable once, at single-threaded startup,
into the store's `environment` layer, and scrubs what it found (see
[[invariants/a-grant-never-reaches-its-own-key|a-grant-never-reaches-its-own-key]])
so neither a child of a tool call nor the session itself can read a live key
back. The sweep is therefore un-repeatable by
construction, and that layer never changes after it.

The **other** layer is the computer's own credential manager. [[map/synod|synod]]
was built for it; exarch reads it too, at startup in `lib.rs` through
`CredentialStore::read_vault`, an ordinary second call that scrubs nothing,
and writes it through the shared `Wallet` (below). **The environment's key
outranks the vault's**, in both products: a variable set for one run is the
narrower, more deliberate act, and a vault key beneath one is kept and shown,
not discarded. A vaulted key, unlike a scrubbed variable, is somewhere a child
could ask for it: a confined child cannot, since the macOS profile admits no
`securityd` and the Linux envelope binds no session bus unless a grant reads
`/run/user`, but an unconfined session can, like any process of the user's.
Each launch reads the vault once per key-bearing account:

- `provider/keychain.rs` — `Keychain::for_app(App)` reaches the macOS
  Keychain, the Windows Credential Manager, or a Linux desktop's Secret
  Service through one `keyring` entry named `(app, account-id)`, so two
  products' keys are two entries exactly as their config directories are two
  directories. Only key-bearing accounts reach the vault, and their ids *are*
  their service names, so an entry name stays short printable ASCII and no
  login account id ever becomes a Credential Manager target.
  `Entry::store_status()` is asked first; a computer with no
  credential manager falls back to an owner-only file beside the app's
  configuration, and `vault()` answers where secrets actually land in a
  sentence a window prints verbatim rather than implying a protection that is
  not there. A blank or control-bearing entry reads as *no key*, the rule
  `credential.rs` already applies to a key read from the environment.
- `provider::credential_files` — every file on the computer holding one of our
  credentials: each app in `app::APPS` contributing its keychain
  fallback path plus `oauth::token_path`. `policy::for_invocation` denies all
  of them to every grant that attenuates the filesystem at all, since the
  base profiles read the `` `xdg-config ``/`` `xdg-state `` places wholesale and would otherwise
  hand the agent the key that pays for its own turn — see
  [[invariants/a-grant-never-reaches-its-own-key|a-grant-never-reaches-its-own-key]].
- the OAuth token store and that fallback file are both written through
  `ral_core::own`'s `OwnDir::store` with `Visibility::Secret`: one implementation
  of the owner-only promise, in the walk, instead of two copies.

The store's mid-session mutators exist for an accounts screen: `known` (every
account, bound or not, since one with no key is precisely the one a user has
come to give a key to), `declare` (a declared endpoint's arrival),
`save_key`/`forget_key` (the vault layer only; the environment's is fixed),
and `retire` (drop an account entirely: a withdrawn declaration, a signed-out
login). `from_environment` and `from_vault` read the two layers apart, so a
screen can say which key is in force and which lies beneath it without asking
the vault again. See
[[decisions/260807_synod-keeps-its-own-accounts|synod-keeps-its-own-accounts]],
whose order this reverses.

`provider/accounts.rs` holds the rest of what an accounts screen needs:
`declared_endpoints`, `declare_endpoint`, `withdrawable`, `checked_key`, a
`find` that resolves by `AccountId` alone, and the plain `Entry`/`Source` row
derivation (`entries`) both products draw from: the source in force
(`Environment`, `Vault`, `Keyless`, `SignedIn`, or `None`), the last four of
its key, the last four of a vault key the environment's outranks
(`shadowed`), and the variable a key is read from. `wallet.rs`, above both
`config` and `provider`, holds **`Wallet { keychain, declarations, label }`**:
the one route through which a product changes its keys and declarations
(`set_key`, `forget_key`, `add_endpoint`, `forget_endpoint`), each checking
everything before writing anything, touching the vault with the store
unlocked, and applying the change live through `Holdings::change`.
`Wallet::exarch()` is `Keychain::for_app(EXARCH)` over `config::path()`, read
at startup as well as written by `/providers`; synod's `accounts::wallet()` is
its own. An OAuth login is no wallet's: the token store is exarch's whichever
product signed in. `Bureau::sign_out` removes the token and retires the local
account; the caller runs `oauth::revoke_blocking` off its UI thread and reports
issuer acceptance, local-only removal or revocation failure without conflating them.

`config.rs` is likewise generalised: `load_declared(path, label)` reads either
product's file, and `save_declared` writes the same `.ral` source back — a
file a program wrote and a person can still edit, which is the whole reason
declarations are `.ral` and not an opaque blob. A declaration is an address,
a protocol, and at most `` key: `none ``; anything else under `key` is refused
without being echoed, since what was written there may be the key itself. A
label or address carrying a quote is refused with a question rather than
escaped.

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
- An **OAuth** login branches to `build_oauth_client`, using its service's
  adapter (`OpenAIResp` for Chatgpt/Codex, `Xai` for Grok). The resolver reads
  `OAuthToken::auth_data` live from the shared cell. Chatgpt/Grok supply the
  bearer; Codex supplies a backend Responses URL override and
  `request_headers`, including the raw backend account id, beta, originator
  and user-agent headers. Other grants never receive Codex headers.
  `refresh_cell_if_stale` is shared by inference and catalogs, dispatching
  refresh by grant and upserting just that full account id. Issued identity,
  Chatgpt client id and Codex raw backend id stay pinned; naming metadata may change.

## One owner for construction

`Provider::build` needs an `Engine` and a `Credential`, and a `Provider`
retains neither — it keeps a built `Transport`. So minting a *second*
selection has nothing to work from unless the engine and the application's
`Holdings` — credential store, model catalog, allowance record, whose lock
discipline is stated once on the type — are reachable together.
`provider/bureau.rs` names them once: `Bureau::Live { engine, holdings }`, or
`Bureau::Scripted` for a session that mints nothing (`Engine::new` primes the
pricing catalog over the network, so no unit test may build one). It mirrors
`Provider`'s own `Backend::{Live, Scripted}` split rather than inventing a
second idiom.

`Bureau::build` is **the only caller of `Provider::build`**, which is
`pub(crate)` for that reason; `available`, `admit` (a `/login`'s admission)
and `with_catalog` are the rest of the surface. The two halves are shared
(`Arc<Mutex<_>>`) rather than owned, so two hosts compose: exarch builds one
bureau over its own pair in `run()`, while synod keeps the same pair as
application-wide window state and mints an engine per conversation. They are
two mutexes and not one so that a credential read never queues behind a
catalog fold, which writes the disk cache under its lock.

**Lock discipline**, synod's own rule inherited: locked briefly, never across
a network call, a picker frame, or a machine boot; and the converse, that a UI
thread never holds either while waiting on an agent thread. `Bureau::admit` is
the one call that takes both at once, store first. The `/model` overlay's
`drive` therefore takes the catalog per fold — to open the `Listing`,
to pump it, to record endpoints, to clone the fetch seam — and never around
the fetch itself; `Bureau` carries `Arc<Bureau>` on `RootConfig` and from
there on the fleet's `Launch`, the one place a run's host settings live,
beside `factory`.

## Model catalogs

**An account's own listing is the authority on what it serves.** The picker
offers only listed models, `--model` alone resolves to the one account whose
listing names it, and `Bureau::build` refuses any other choice, so no account is
ever asked for a model it does not list. `ModelCatalog` memoises and
disk-caches both paths:

- **Roles** (`provider/roles.rs`) are the one way a spawned agent gets a
  model other than its parent's, and the human binds them. `--role NAME=MODEL`
  (repeatable) resolves MODEL exactly as `--model` does through
  `state::named` — to the one account whose listing names it, or to the account
  `--role-provider NAME=ACCOUNT` gives (ACCOUNT as `--provider` accepts it),
  the tie-breaker the ambiguity error asks for; `--role-via NAME=TAGS` gives
  the upstreams that serve it (TAGS as `--via` takes them). Each of the three
  flags refuses a name given twice, and the latter two a name no `--role`
  binds. `Roles::mint` builds each `Choice`
  through `Bureau::build` once, at launch, with the launch's `Tuning`
  (`Tuning::initial` plus `--effort`) and `--max-tokens`; a later `/model`
  changes only the switching agent's own provider. Roles are flags only, never
  persisted, and absent by default: with none, every child inherits and the
  system prompt says nothing of roles. `Roles` rides `RootConfig` into
  `Launch`; the desk's `child_provider` resolves `` `role <name> `` against it,
  and the `Roles` prompt section (`data/roles.md` plus one `name: model` line
  per role) is taught only to an agent that spawns. The agent never lists
  models or names an account: the catalog is not an ambient authority.

- Anthropic, DeepSeek and Gemini list through exarch's own `GET /models`
  (paged to the end for Anthropic and Gemini), because each reports a context
  window genai discards; other API-key providers list through genai's
  `all_model_names`. Each of the three is one `Native` row in
  `models/native.rs` — base URL, how the key is presented, paging, page
  parser — and `Native::of` is the only place an adapter picks one. Every
  listing is `Vec<Listed { id, context_window }>`, and the picker sees only
  the names.
- `models::list_login` dispatches after the shared stale-token check.
  `plan_models` preserves server order, `visibility == "list"` and context
  windows for Chatgpt's `https://api.openai.com/v1/models` and Codex's backend
  catalog. Codex sends its raw-account headers and `client_version`, pinned to
  `0.164.0` unless `EXARCH_CODEX_CLIENT_VERSION` overrides it. Grok lists through
  genai's Xai adapter. Its OAuth `/v1/models` acceptance remains live-unverified.
- `Bureau::build` is the one place a selection passes, and takes a `Choice`:
  an account, one of its models, and the `Via` it is served through, if any.
  It refuses a via on a service that does not route, or one naming an upstream
  the model's endpoints do not list; a via's window is the least its
  upstreams hold, an upstream listing none holding the model's. A plan account
  (`Service::shares_a_plan`) gets a selection carrying reasoning effort alone:
  the token-sharing route refuses `temperature`, `top_p` and
  `max_output_tokens`, so none rides one. It reads the serving
  account's listing once (`models::listing_of`: the cache, else one fetch made
  with the catalog unlocked), refuses a model the listing does not name, and
  resolves the context window from it: the listing's own figure, else the
  OpenRouter catalog, else unknown. `Provider` owns the
  value; the record carries it (`SessionStarted`, `SessionResumed`,
  `ModelChanged`), so a tab's fold reads it without a catalog. The status line
  draws an unknown window as `?`.
- `/login` admits an account mid-session through `provider::admit_login`,
  which runs `CredentialStore::add_oauth` inside `Holdings::change`: every
  change to the store goes through that one route, which hands the catalog the
  store's fresh `Roster` (`ModelCatalog::set_roster`), so a key saved, a key
  forgotten and an endpoint withdrawn reach the listing as a login does.
  Re-login updates the cell in place; a login that has since learnt its email
  is renamed where it stands, its identity being the account id throughout.
- OpenRouter serving endpoints remain a separate request per model; the
  `/model` overlay makes it for whichever OpenRouter model is highlighted, and
  `Bureau::build` for a model minted with a via. `ProviderEndpoint::tag` keeps
  the full endpoint tag, including variant or region suffixes. A `Via` is an
  ordered, non-empty, duplicate-free list of those tags, most preferred first,
  sent as `provider.order` with `allow_fallbacks: false`: every request is
  served by an upstream the via names, or by none. `--via`, `--role-via` and
  the picker's via row (one tag) choose one; without one, OpenRouter chooses
  per request. The picker shows the suffixes and keeps one row per distinct
  tag, in server order: selection resolves by tag, so repeated tags would trap
  navigation.
- `listing.rs` states the picker-side orchestration once for every front-end
  (the `/model` overlay and synod's window alike): the `FetchState` vocabulary,
  a keyed background-fetch pump (`Fetches`), and the per-provider `Listing`
  that seeds from the catalog's cache and fills misses in as they land.
- `state.rs` remembers the *choice* — account, model and via — as the
  project's `state.json`; tuning is never replayed: a remembered effort can be
  one the new model rejects, and the flags and the picker set it. A restored
  via is rechecked against the model's endpoints like any other.
  `opening(flags, saved, available, listing)` decides what a launch opens on
  (`Opening::Named` from the flags, `Restored` from the saved choice, else
  `Choose` — the picker); a refused restore opens the picker instead of ending
  the launch or, headless, fails.

## The streaming path

- `complete(request, on_delta, cancel)`, a `Request { system, transcript,
  tools, search }` —
  streams one assistant reply, calling `on_delta` with a `Delta::Say` per text
  chunk, a `Delta::Think` per reasoning chunk, and a `Delta::Retry(&ProviderError)`
  when an attempt failed and is being re-issued (the deltas before it belong to
  the failed attempt, the ones after to the new), and projects the `StreamEnd`
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
- **Liveness is one byte-level clock, owned by genai.** `complete_options` sets
  `stream_header_timeout` (`STREAM_HEADER_TIMEOUT`, 3 min) and
  `stream_read_timeout` (`STREAM_READ_TIMEOUT`, 10 min, per raw body chunk, SSE
  comments included) on every request, and the stream loop races only the cancel
  token against `next()`; a `Heartbeat` event adds nothing there. The bound is
  patient because HTTP/2 PING and TCP keepalives (`provider/tls.rs`) evict a
  dead peer in about 45 s, so it only judges a live server gone quiet.
  reqwest's `READ_TIMEOUT` stays as the backstop for every client sharing it,
  held 30 s above the read bound so genai's named `StreamTimeout` wins; `Fault`
  classifies it `Transient`. This lands the slice of
  [[decisions/260702_provider-heartbeats-and-retry-boundaries|provider-heartbeats-and-retry-boundaries]]
  that fits genai as released: no fork, no semantic clock.
- Streaming is the **only** call the engine makes. There is no second,
  non-streamed path: bounding the window is
  [[map/exarch/agent|`Avatar::evict`]]'s job and it makes no provider call at
  all ([[decisions/260906_context-rollover|context-rollover]]).
- `cancel` is the **request-local** cancellation handle: the foreground exchange
  passes its root token (Esc-linked), an async `agent` passes its own
  `cancel::Token`, so two concurrent requests never share one
  process-global slot —
  the provider-side seam of [[decisions/260617_async-agent-tool|async-agent-tool]];
  the token and inbox belong to [[map/exarch/tools|tools]] and
  [[map/exarch/agent|agent]].

## Retry driver

Both paths in `provider/stream.rs` run on a tokio runtime through **one retry
driver**, `provider/retry.rs::retry_with_backoff`, over an `Attempt<T>` (`Done`
/ `Failed`):

- The streaming-specific rule rides on `Done`: only answer text survives into
  the transcript, so once prose has flowed to `on_delta` a re-issue would
  double it. A failure then commits as `stalled_step_out`, a `CutShort::Stalled`
  `StepOut` returned as `Attempt::Done`, *only if* `ProviderError::retryable`
  (the driver's own verdict, `retry.rs::budget`) says it would be retried; any
  other error surfaces as itself. A failure before any prose, reasoning alone
  included, goes to the ordinary retry loop. No third "don't retry" variant is
  needed. `CutShort` lives in `provider/error.rs`
  beside `ProviderError`, whose `Truncated` carries it: one type says why a
  turn was cut short, and each arm carries its own remedy
  ([[internals/provider-fault-recovery|provider-fault-recovery]]).
- Each attempt is handed the failure it replaces (`None` first), which the stream
  emits as `Delta::Retry { cause, lost }` before the new attempt's deltas
  (`lost`: the usage of an attempt that streamed output the provider never
  reported): the commit producer seals the failed attempt's lanes there and
  `deliberate` records the lost usage and a `Forensic::Retried`, the log's
  alone, drawn by no renderer.
- Rate limits get a larger budget and a higher backoff ceiling than transient
  failures, and a named wait is honoured. A `Refused` whose
  reset lies past that ceiling (`Recovery::Deferred`) surfaces on its first
  attempt, and the terminal trunk resumes it by a wakeup at the reset
  ([[internals/provider-fault-recovery|provider-fault-recovery]]).

Transport retry lives here, so the [[map/exarch/agent|nudge]] rules cover
only model-behaviour outcomes, not transport.

## Structural error classification

`ProviderError::from_genai` (`provider/error.rs`) **reads retryability from
genai's typed variants, not from its `Display` string.** A single structural walk, `Fault::of`, descends
each error to one of three leaves — `Response` (a provider error with an optional HTTP status), `Transport`
(a `reqwest` fault with no status), or `Terminal` (no recoverable leaf) — recovering
the `StatusCode`, the response `HeaderMap`, and the parsed JSON body across the four
paths a non-2xx reaches us by: `HttpError`, `WebModelCall(ResponseFailedStatus)`,
the `HttpError` boxed inside a streaming `WebStream` (recursion), and a mid-stream
`ChatResponse` frame whose numeric status, when present, lives in
`body["error"]["code"]` / `body["code"]`. A string code or absent status
still retains the complete body and becomes a terminal `Api` error, subject
to the same overflow classification as HTTP errors.
The status drives the split: `refused` reads a 429 as a spent quota (`Api`) or
else a `Refused` stated whole — its `Limit`, reset and receipt — taking the
first two from `provider/reset.rs`, which reads every header and body
convention providers name one by, and leaving recovery to `Refusal::recovery`;
a 5xx is `Transient`, any other 4xx `Api`.
The `_ => Terminal` floor makes the walk total — a contract breach (a non-JSON
2xx) or an unrecognised shape surfaces raw rather than being retried on a
`Display`-string guess. The full
tutorial is [[internals/provider-fault-recovery|provider-fault-recovery]]
([[invariants/transcript-admission|transcript-admission]]).

A 4xx whose text matches a known overflow phrasing (vLLM, OpenAI/OpenRouter,
Anthropic, Gemini), or whose `type`/`code` is `context_length_exceeded`, is
`Overflow` rather than `Api`: it carries the `sent` and `limit` token counts
when stated, and is never retried by `retry.rs`. A stated `limit` narrows the
`Provider`'s interior-mutable window (it only ever shrinks, and dies with a
`/model` swap), recorded as `Forensic::WindowNarrowed` so every reader agrees;
`Avatar::deliberate` then sheds the older half and retries the turn once.
A resume replays the learned window for the same model; a model minted with
a via starts at the least window its upstreams hold.

Each retryable and 4xx variant carries the parsed body (boxed at the error
boundary) as an optional JSON value, so the renderer can print a
labelled, structured error from the JSON rather than scraping the cause text.
`record::fault::body_fields` keeps `type` and `code` as distinct fields, with
`message` last; `summary` carries the message across agent boundaries. The
complete body remains in the recorded error, including its stream envelope;
the chrome lives in [[map/exarch/cards|cards]] / [[map/exarch/frontend|frontend]].
`reset.rs`'s last-resort text scrape, `parse_retry_after`, slices the
*lowercased* copy it searches, never indexing the original with an offset taken
from it, so a length-changing lowercase (`İ`) cannot land mid-character and
panic (X8).

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
- `Usage::unreported` counts turns whose usage the provider never sent (a
  stalled stream, an `End` without usage). It adds like every other field, a
  total with any is a lower bound, and the cost slot says so (`$0.0364 + ?`).
  `Usage::measured_input()` is the one reading of `input` as a context
  measurement and is `None` for such a usage, so neither the pressure gauge's
  `Measure` nor the view's `last_input` is ever overwritten by a zero.
  Headless `total_cost_usd` is `null` and `usage.unreported_turns` is set
  likewise.
- `provider/pricing.rs` fetches OpenRouter's `/api/v1/models` catalog once per process
  and caches it; `ModelPricing::dollars` strips the cache counts out of `input`
  and bills uncached/cache-creation/cache-read/output each at its rate, falling
  back to the base input rate when no separate cache rate is published. Native
  DeepSeek models use the local rate table — DeepSeek's own first-party card,
  doubling inside its weekday UTC peak windows, where the catalog would price
  a third-party OpenRouter host — before any OpenRouter alias; other providers
  use the catalog. The catalog also supplies `ModelCaps` (supported request
  parameters, and the context window only as the fallback when the serving
  account's listing reports none) for startup and picker decisions. Offline
  starts degrade to `—`.

## The wire request

`provider/wire.rs` is the one place an owned `genai::ChatRequest` is built —
[[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]] is the
ADR, [[internals/session-record|session-record]] covers the persistent
`render_messages` projection it consumes. `Sealed(ChatRequest)` wraps the result and is
deliberately not `Clone`, so a built request cannot be kept alive and cloned
for a later retry; `manufacture(adapter, system, transcript: &[ChatMessage],
tools)` is called fresh inside each retry attempt in `stream.rs`'s
`Engine::complete`, and is the only place a request is built.

`manufacture` marks `cache_control: ephemeral` breakpoints on the system
prompt and the last two messages, for Anthropic alone: two anchors of
different depths, so a diverging tail (retry, fork, a self-nudge) still lands on
a cached prefix. The marks land on `manufacture`'s own fresh clone of the
transcript's shared messages, never on the shared segments themselves. On
OpenAI the same marks would select the metered *explicit* prompt cache, which
the ChatGPT plan route rejects outright (rust-genai #273),
so every other adapter carries its system prompt in the request's own
`system` field — where the Responses adapter's `instructions` string wants it
anyway — and leans on the per-process `prompt_cache_key` alone, which genai
reads as intent for the free implicit cache. `tool_defs` also receives the
selected `Service`, so hosted search can distinguish the two Responses routes.
Alongside the agent's [[map/exarch/tools|`Toolset`]], search is carried on
`OpenAIResp`, `Anthropic` and `Gemini`; only Codex receives the legacy
`external_web_access: true` configuration. Chatgpt retains its default search
form. The bit is [[map/exarch/agent|agent]]'s `search`.
