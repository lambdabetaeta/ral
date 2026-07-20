---
status: active
---

# A shared transport beneath the per-agent providers

**The tokio runtime and genai clients belong to the *fleet*, not to each
`Provider`. A `Provider` should be a cheap per-agent *selection* (provider id +
model + max-tokens + tuning) that borrows one process-wide transport, so a
`/model` switch or a divergent per-agent model is a re-selection, never a fresh
runtime.** [[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]] already
gave every `Agent` its own hot-swappable `ProviderHandle` (`agent.rs:63`,`192`),
so per-agent (and per-provider) models are live: `/model` swaps the focused
agent's handle alone (`tui.rs:3496`), a `fork` seeds the child's own handle from
the parent's current provider (`agent.rs:450`). What is *not* yet shared is the
transport underneath. This decision closes that, and points the banner/`ctx%`
chrome at the focused agent's live provider while it is there.

## The fusion that remains

`Provider` still fuses a heavyweight transport with a one-line selection
(`provider.rs:1042`). Its `Live` backend (`provider.rs:1065`) owns:

| Piece | Per-agent? | Cost |
|---|---|---|
| `tokio::runtime::Runtime` (multi-thread, `make_runtime` `provider.rs:1094`) | **No** — process-wide | A worker-thread pool per instance |
| `client` (`build_client` `provider.rs:1134`) | Per credential *and wire adapter*, not per agent — the client binds auth and its adapter; the model rides each request (`t.model.model_name` `1153`) | One client per (credential, adapter); an OpenAI key spans both adapters, every other provider keeps one |
| `cache_key` (`fresh_cache_key` `provider.rs:1090`) | **No** — per-process routing hint | — |
| `token_cell` `Arc<Mutex<OAuthToken>>` | **No** — per-login, shared by clone | — |
| `flat_rate` | tracks the credential | — |
| `model` + `max_tokens_override` + `tuning` | **Yes** — the genuinely per-agent bits | trivial |

A `fork` shares the parent's runtime because it clones the `Arc<Provider>`
(`agent.rs:450`) — good. But `Provider::build` (`provider.rs:1215`) calls
`make_runtime()`, so **every `/model` switch mints a fresh multi-thread tokio
runtime** (`tui.rs:3487`), and N agents on N distinct models means N runtimes.
The selection is the cheap part; the transport is the expensive part. Split along
that line.

## The split

### `Engine` — one per process, owned by the `Fleet`

The `Fleet` (`fleet.rs:28`) already holds what is shared — the registry, the one
bus, `focus`, `interactive`. The transport is the same kind of thing, so it lives
there too (as `Arc<Engine>`, cloned into nodes that need it):

```rust
struct Engine {
    runtime: tokio::runtime::Runtime,
    cache_key: String,
    transports: Mutex<HashMap<TransportKey, Arc<Transport>>>, // pre-warmed
}

struct Transport {
    client: genai::Client,
    token_cell: Option<Arc<Mutex<oauth::OAuthToken>>>,
    flat_rate: bool,
}
```

`TransportKey` is `(credential, adapter)` — the two things that actually fix a
genai client, which binds both its auth and (via `with_adapter_kind`) its wire
adapter. Most providers resolve a single, model-independent adapter per
credential, so the map holds one entry each; an OpenAI key is the exception —
`gpt-4o` speaks chat completions and `gpt-5` the Responses API — and so warms one
client per adapter, never sharing a chat-pinned client with a Responses model.
The adapter is *not* stored in `Transport`'s body, and the request-shaping
adapter is still recomputed from the model at request time via
`adapter_for_provider_model`, so the engine carries no per-model map and needs no
update when a new model appears.

### `Provider` — per agent, cheap, the existing hot-swappable handle's payload

```rust
struct Provider {
    engine: Arc<Engine>,        // borrowed transport, not owned
    id: ProviderId,
    model: String,
    max_tokens_override: Option<u32>,
    tuning: Tuning,
}
```

`Provider::complete` resolves its `Transport` from `engine.transports[key]` and
runs on `engine.runtime` — the `self.runtime.block_on(...)` calls at
`provider.rs:1427`/`1552` and the OAuth refresh at `1378` move to
`engine.runtime.block_on(...)`. `Provider::build` becomes cheap: it ensures the
transport exists in the engine and stores the selection — **no `make_runtime`**.
A `/model` swap builds one of these sharing the same `engine`; the per-switch
runtime churn disappears. The throwaway `make_runtime()` at `provider.rs:2446`
   folds into the engine too. `prime_pricing` (`provider.rs:1218`) fires once at engine construction, not per-provider-build.

### Concurrency is already proven

Sibling peers already run on detached threads against their own `Arc<Provider>`
handles, and `runtime.block_on(&self, …)` can be invoked from many threads at once —
each agent's thread parks in its own `block_on` while one worker pool drives all
their HTTP futures. Conversation state lives in each `Agent`'s `Transcript`, never
in the transport, so sharing one runtime across the fleet is sound (and strictly
better than `N × num_cpus` workers). `token_cell` refreshes serialise under its
mutex; the shared `cache_key` is a routing hint, harmless across distinct
conversations.

## The banner / `ctx%` fix (same theme: read the live provider)

`/clear` redraws the banner from the **frozen** `SessionInfo` (`tui.rs:724`,
`1913`, built once in `lib.rs:216`), `App::context_window` is set once at
construction (`tui.rs:639`), and a `/model` swap updates only `status_model`
(`tui.rs:3497`) — not the window. So after a switch: the `ctx%` gauge is stale,
and a subsequent `/clear` repaints the *old* model.

Fix: source the live model and caps from the **focused agent's provider** —
`fleet.agents.provider(focus).current()` → `model()` + `caps_for(model)`
(`provider.rs:42`) — for the banner, the status line, and the `ctx%` cap.
`SessionInfo` keeps only its genuinely static fields (`system_size`, `base`,
`extend_base`, `restrict_files`, `scratch`, `cwd`); the model-derived fields
(`provider`, `model`, `canonical_slug`, `context_window`, `max_output_tokens`)
leave it. The window also follows `TAB` focus changes, since each agent may run a
different model.

## Settled

1. **Pre-warm transports** — one per available credential at startup and on a
   `/model` to a new credential. The registry is then immutable on the hot path,
   shared lock-free across the peer threads; the `Mutex` is touched only on the
   rare warm.
2. **`Engine::Scripted`** mirrors `Provider::scripted` (`provider.rs:1242`, no
   runtime) so the `agent.rs` test seam (`scripted(...)`, `1382`/`1520`) keeps
   working.
3. **The banner reads the focused agent's live provider**, not `SessionInfo`.

## Implementation order

Each step compiles.

1. **Extract `Engine` holding one transport, behaviour identical.** Move the
   runtime + cache_key + the one client into an `Arc<Engine>`; `Provider` holds
   `Arc<Engine>` + selection; `complete` runs on `engine.runtime`. Build the
   engine once at startup (`lib.rs:199`) and propagate it where `Provider::build` is
   called. Pure refactor — still one credential.
2. **Hang the `Engine` off the `Fleet`** (`fleet.rs`), cloned into agents via the
   `ProviderHandle` path; `/model` (`tui.rs:3487`) builds a cheap `Provider` on
   the shared engine instead of a fresh runtime.
3. **Repoint the chrome** — banner / status / `ctx%` read the focused agent's
   `provider().current()`; strip the model-derived fields from `SessionInfo`.
   Closes the stale-`/clear` and stale-gauge bugs.
4. **Pre-warm all credentials + dedupe transports** per `TransportKey`. Divergent
   per-agent models now cost one selection each, one runtime total.

## Consequences

- One tokio runtime for the whole fleet; `/model` and divergent per-agent models
  stop minting runtimes.
- `Provider` is a cheap value; the `Arc<Provider>` in each `ProviderHandle` stops
  carrying a thread pool, so hot-swaps and forks are near-free.
- Live model display has one source of truth — the focused agent's provider — so
  banner, status, and gauge cannot disagree, across `/model`, `/clear`, and `TAB`.
- `SessionInfo` becomes a static snapshot only.

See also [[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]],
[[map/exarch/provider|provider]], [[map/exarch/agent|agent]].
