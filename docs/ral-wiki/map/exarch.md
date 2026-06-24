---
generated_at_commit: 3e5ce15
generated_at_date: 2026-06-24
covers_paths: [exarch/src/main.rs, exarch/src/cli.rs, exarch/src/bootstrap.rs, exarch/src/credential.rs, exarch/src/prompt.rs, exarch/data/system.md, exarch/data/ral.md, exarch/data/script-style.md]
---

# Map: exarch

exarch is a small LLM coding agent — a separate workspace binary that
**embeds** [[map/core|ral-core]] (`ral-core = { path = "../core" }`). A model
(Anthropic / OpenAI / OpenRouter / DeepSeek, or a signed-in ChatGPT account) is
given one `shell` tool, and each call is evaluated as a *ral top-level turn*
against a persistent in-process `Shell`, under capabilities the user chose. It
ships as one executable ([[invariants/single-binary|single-binary]]); the same
binary re-execs itself as the [[map/core/capabilities|OS sandbox child]].

The agent is a provider loop over one tool, each turn grant-framed — the
design argument lives in [[design/exarch-architecture|exarch-architecture]].
This page maps the binary's *front door*: what runs at startup, and how the
system prompt is assembled.

## Entry and dispatch

`run` (in `lib.rs`, lifted out of `main` so integration tests link the whole
crate) is the session path; `main.rs` is the thin shell over it.

- **Pre-`main` trampoline.** Before any setup, `dispatch_pre_main` short-circuits
  a re-exec child, returning `Option<u8>`: `install_child_hooks_and_serve_helpers`
  (set the child-shell extension that dresses a sandbox-IPC child with exarch's
  host builtins, then serve the pipeline-stage / test-helper re-execs) `.or_else`
  the OS-sandbox stage ([[map/core/capabilities|`serve_sandbox_early_init`]]).
  `main` and **every test `#[ctor]` run this identical function** — they differ
  only in how they act on `Some` (exit vs return the `u8`). A test binary reaches
  `main` only through libtest yet is the same
  [[invariants/single-binary|multicall executable]] a child re-execs; skip the
  sandbox stage and the confined transport stays unpinned, so that binary's
  confined-path tests cannot run.
- **Subcommands** (`cli.rs`) run an out-of-band action and exit before any session
  setup: `login` / `logout` / `accounts` manage signed-in ChatGPT accounts (see
  below), `--model` and the session flags are ignored on this path.
- **Session.** Absent a subcommand, `run` resolves the initial provider+model,
  composes the capability lattice (`policy::for_invocation`, → [[map/exarch/policy|policy]]),
  assembles the system prompt (`prompt::assemble`), builds the trunk
  [[map/exarch/agent|`Agent`]] + `Provider`, and hands off to one frontend — the
  inline TUI or, under `--headless`, the pipe-friendly headless runner — which
  wraps the trunk's shared handles in a [[map/exarch/agent|`Fleet`]].

## Accounts

Several ChatGPT subscriptions can be signed in at once; each is its own
selectable provider, switched in the `/model` picker exactly like any API-keyed
provider — there is no second account dimension.

- `login` adds or refreshes one account (opening a browser, or `--device-auth`
  for a headless host).
- `logout` removes one account by email or id, or `--all`.
- `accounts` lists the signed-in set.

The token store holds a list of logins keyed by account id; `run` resolves each
into its own OAuth-backed provider, ordered after the API-key providers.

## Credentials and the env scrub

`credential.rs` resolves every provider's secret once at startup and **scrubs
the key variables from the process environment, so no child a tool call spawns
can inherit a live key.**

- **`CredentialStore::resolve_and_scrub`** sweeps every known provider — the
  famous `ProviderKind`s and the `custom` providers from `config.ral` — reading
  each one's conventional key variable (`key_env`) into the in-memory store, then
  removing from the environment *every key variable that was present*, whether or
  not it yielded a usable key. A malformed value (a pasted newline) is still a
  live secret, so it is swept too.
- The scrub makes resolution **eager**: once a variable is gone it cannot be
  re-read, so a key absent at startup stays absent for the run. Read and removal
  happen while the process is still single-threaded — before any session worker —
  so the env mutation cannot race.
- A signed-in **ChatGPT account** never touches the environment: its login is
  loaded from the OAuth token store into an `OAuth` credential cell ([[#Accounts]]),
  a distinct identity from an `OPENAI_API_KEY` provider, so the two coexist.
- This is the subtractive half of exarch's env shaping; the additive half —
  `NO_COLOR`, `$EXARCH_SCRATCH`, and the redirected tool homes — is seeded onto the
  session shell ([[#Bootstrap]]). Per-spawn loader-variable hardening (stripping
  `LD_PRELOAD`/`LD_AUDIT`/`LD_LIBRARY_PATH` under an active grant) is [[map/core|ral-core]]'s,
  not exarch's. The *why* is [[decisions/260613_provider-config-ral-script|provider-config-ral-script]];
  the boundary a child actually inherits is ral's [[design/grant|grant]].

## Bootstrap

`bootstrap.rs` holds the once-per-process pieces; nothing here is per-turn.

- **`boot_shell`** — the one constructor that may boot a session shell: clear
  stale ral interrupts, install ral's handlers, chain exarch's cancel over them,
  call core's [[map/repl/startup|`ral_core::driver::boot_shell`]], then layer
  exarch's host builtins and source the `agent.ral` library, register its docs,
  suppress ANSI colour at the source, and seed the exit hints.
- **Machine probing** — `host::snapshot` formats the live machine into the
  prompt's `Host` section over core's `ral_core::host` probes (`os`, `now`, `cwd`,
  `user`, `home`, `git`), best-effort: a missing value drops its line.
- **`Scratch`** — the disposable per-session directory exposed as
  `$EXARCH_SCRATCH`, with the legacy build-tool homes (`CARGO_HOME`, …) redirected
  into it so a write lands inside the grant rather than in a denied real cache.
- **`log_run_dir`** — the durable per-run [[map/exarch/frontend|session-log]]
  directory at `$XDG_STATE_HOME/exarch/<project>/<run>/`, keyed by a slug of the
  project cwd (`project_slug`), so logs survive an abnormal exit. The persisted
  model selection (`state.json`) lives under the same per-project `project_dir`.
- **`xdg_app_dir`** — the one spelling of the `$XDG_<kind>_HOME/exarch/` convention
  that `project_dir` (state), the model cache (cache), and the trusted config home
  all build on ([[design/exarch-config-dir|exarch-config-dir]]).

## System prompt

`prompt::assemble` builds the prompt from `(heading, body)` sections walked by one
uniform renderer, in order **persona, `Grant`, `Host`, `Ral`, `Script style`,
[`Workspace`], [`Headless`]**.

- **Persona** (`data/system.md`, unheaded — it sets the tone, not a topic) frames
  the session as a *progressively expanding script of reusable definitions*: the
  agent saves what it searches for in a definition, and definitions, working
  directory, and concurrent threads persist across turns.
- **`Grant`** sits directly under the persona, before the tool reference tempts
  the model to reach for authority it lacks: a static legend (`data/grant-legend.md`)
  over the live capability bullets (or one ambient-authority line when nothing is
  attenuated). The set of builtins is *not* listed — the agent discovers it at
  runtime with `help`, so the prompt cannot drift.
- **`Ral`** (`data/ral.md`) is the language reference, framed around definitions
  rather than bindings; its handler docs follow the lambda-only install rule
  ([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]):
  per-command `handlers:` entries are unary `{ |args| … }`, the catch-all
  `handler:` binary `{ |name args| … }`. Its `## Surfacing` section teaches the
  five Bertin marks and the role set ([[decisions/260619_surface-carries-documents|surface-carries-documents]],
  → [[map/exarch/cards|cards]]).
- **`Script style`** (`data/script-style.md`) is the reuse guide: one program, not
  a nervous probe — define then query, parameterised blocks, records for knobs,
  blocks as policy, long-running work behind `spawn`/`await`.
- **`Workspace`** (`discover_agents`) collects the `AGENTS.md` instruction files,
  outermost first so the deepest file's recency wins: the operator's
  `<config>/AGENTS.md`, then every repo `AGENTS.md` from the git root down to cwd
  (the walk stops at the first `.git` entry; outside a repo, only `cwd/AGENTS.md`).
  Present whenever any is found; project guidance that cannot widen the `Grant`
  ([[design/agents-md-injection|agents-md-injection]]).
- **`Headless`** (`data/headless.md`) is appended last under `--headless`, where
  recency carries, warning that assistant prose now *is* the output.

`--system FILE...` collapses the persona, `Ral`, and `Script style` slots into one
user-supplied section (the user takes responsibility for the tool reference);
`Grant`, `Host`, `Workspace`, and a headless `Headless` still surround it.

## Subsystems

- [[map/exarch/agent|agent]] — the uniform node and the thin `Fleet`: the turn loop
  (provider round-trips, tool dispatch with prompt-queue steering, auto-compaction,
  the nudge-retry policy, sub-agent fork), the `parent` predicate, the owned
  hot-swappable provider, dynamic focus, and the subtree cancel cascade.
- [[map/exarch/provider|provider]] — LLM transport over genai: streaming, the retry
  driver, prompt caching, usage and dollar accounting.
- [[map/exarch/shell-eval|shell-eval]] — one tool call as a ral top-level turn under a
  pushed capabilities frame; the streaming digest and the surface host sink.
- [[map/exarch/policy|policy]] — capability composition (base ∨ extend ⊓ restrict) and
  the bake-in profiles; the boundary *is* ral's [[design/grant|grant]].
- [[map/exarch/tools|tools]] — the tool registry: `ral`, the universal spawn family, `reply`, the schedule family, `fff`; one axis (`replies`) gates the conversing trunk vs every returning agent. The sub-agent model this axis describes is [[design/agents|agents]].
- [[map/exarch/builtins|builtins]] — the resident host atoms: the hash-addressed edit
  primitives and the `agent.ral` helpers ([[design/hash-addressed-editing|why]]).
- [[map/exarch/frontend|frontend]] — the agent/UI boundary (event bus, session log) and
  the two frontends, the inline TUI and headless.
- [[map/exarch/cards|cards]] — the render document `surface` carries: a `card` of closed
  Bertin marks decoded once and drawn by one generic interpreter; open card set,
  closed mark set.
- [[map/exarch/io-surface|io-surface]] — every redirect read/write and exec image as one
  card: core emits an I/O event at the runtime doors, exarch binds it to a mark; the
  closed door set is clippy- and meta-test-enforced.

## Sandbox

exarch does not invent its own sandbox. Each turn is evaluated under a
**profile's capabilities pushed onto ral's capability stack** —
`Shell::with_capabilities(caps, |s| eval_top_level(…, s))` — so the safety
boundary *is* ral's [[design/grant|grant]] mechanism — authority attenuated by
intersection. There is no source-level `grant { … }` the model could escape;
the frame is installed by the host. Profiles ship as `.exarch.ral` files in
`exarch/data/` (`dangerous`, `reasonable`, `read-only`, `minimal`, `confined`);
see `exarch/PROFILES.md` and [[map/exarch/policy|policy]].

## Scheduled wakeups

With `--allow-schedule`, the agent may schedule its own wakeups (`schedule`,
`schedules`, `unschedule`) — a cron expression or a one-shot `after <dur>`. A
wakeup schedules the *agent*, not a worker: at its time a synthetic user turn is
posted to the agent's own inbox and delivered at the turn boundary, re-engaging
the loop with no human present. It is off by default — waking yourself
indefinitely is real authority. The inbox/reaper mechanics live on the
[[map/exarch/frontend|frontend]] and [[map/exarch/agent|agent]] pages; see
[[decisions/260617_scheduled-wakeups|scheduled-wakeups]].

## Where to look

- `exarch/data/{system.md, ral.md, script-style.md, grant-legend.md, headless.md, agent.ral}` —
  the persona rules, ral reference, reusable-script guide, grant legend, headless
  warning, and the embedded agent helper library.
- Provider configuration — a famous provider auto-populates from its env key, an
  unusual one from a hand-written XDG `config.ral`
  ([[decisions/260613_provider-config-ral-script|provider-config-ral-script]]).
- `exarch/README.md`, `exarch/PROFILES.md` — human docs.

[[map/repl|repl]] is the sibling `ral` binary over the same engine.
