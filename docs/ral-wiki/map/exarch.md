---
generated_at_commit: cbeb5457
generated_at_date: 2026-08-17
covers_paths: [exarch/src/main.rs, exarch/src/lib.rs, exarch/src/cli.rs, exarch/src/bootstrap.rs, exarch/src/provider/credential.rs, exarch/src/prompt.rs, exarch/src/agent/build.rs, exarch/src/fleet/desk.rs, exarch/data/system.md, exarch/data/agent.md, exarch/data/agent-spawn.md, exarch/data/ral.md, exarch/data/script-style.md]
---

# Map: exarch

exarch is a small LLM coding agent — a separate workspace binary that
**embeds** [[map/core|ral-core]] (`ral-core = { path = "../core" }`). A model
(Anthropic / OpenAI / Gemini / xAI / Qwen / OpenRouter / DeepSeek / OpenCode, a
`custom` provider, or a signed-in ChatGPT account) is
given one `ral` tool, and each call is evaluated as a *ral top-level run*
against a persistent in-process `Shell`, under capabilities the user chose. It
ships as one executable ([[invariants/single-binary|single-binary]]); the same
binary re-execs itself as the [[map/core/capabilities|OS sandbox child]].

The agent is a provider loop over one tool, each run grant-framed — the
design argument lives in [[design/exarch-architecture|exarch-architecture]].
This page maps the binary's *front door*: what runs at startup, and how the
system prompt is assembled.

## Entry and dispatch

`run` (in `lib.rs`, lifted out of `main` so integration tests link the whole
crate) is the session path; `main.rs` is the thin shell over it. On the way out,
`main` runs `ral_core::sandbox::teardown_session` — deleting the session's
AppContainer profiles on Windows, a no-op elsewhere — before surrendering the
exit code.

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
- **Session.** Absent a subcommand, `run` resolves the initial provider+model
  (`--provider` pins the identity; `--effort` sets reasoning effort; `--chat`
  drops the system prompt and all tools), composes the capability lattice
  (`policy::for_invocation`, → [[map/exarch/policy|policy]]),
  assembles the system prompt (`prompt::assemble`), builds the trunk
  [[map/exarch/agent|`Agent`]] via `Agent::root(RootConfig, RootSeat, provider)`
  — the ral binary seats it on `RootSeat::Identity`; a wire seat drives a
  remote engine instead, and synod reuses the same construction — and hands
  off to one frontend — the inline TUI or, under `--headless`, the
  pipe-friendly headless runner — which wraps the trunk's shared handles in
  a [[map/exarch/agent|`Fleet`]].

## Accounts

**One service may own many accounts.** `chatgpt` is a single service, and a
login email can carry several accounts under it — a personal one and one per
workspace, each with its own issued id. Each account is separately selectable
in the `/model` picker, exactly like any API-keyed one, so there is no second
account dimension; a key-bearing service is simply the case where the service
owns exactly one account and lends it its name. The shape is
[[map/exarch/provider#Services and accounts|`Service` × `Account`]].

- `login` adds or refreshes one account (opening a browser, or `--device-auth`
  for a headless host).
- `/login` performs the same sign-in inside a running TUI session, then admits
  the returned shared OAuth credential to both the live store and model
  catalog; a refresh of the selected account is visible to its next request.
- `logout` removes one account by email or id, or `--all`. An email two
  accounts answer to names neither, so it is refused with both account ids
  rather than taking whichever came first.
- `accounts` lists the signed-in set, each named by
  `identity::label` against the others present.

The token store is an object keyed by the rendering of an `AccountId`, one
entry per account, and so is every map above it. `run` resolves each login into
its own account, ordered after the key-bearing ones. Because a key-bearing
account's id *is* its service name, an existing `state.json`, model cache
entry, or `--provider deepseek` keeps working untouched; only a ChatGPT
selection changes key.

## Credentials and the env scrub

`provider/credential.rs` resolves every provider's secret once at startup and **scrubs
the key variables from the process environment, so no child a tool call spawns
can inherit a live key.**

- **`CredentialStore::resolve_and_scrub`** sweeps every known account — the
  built-in table and the endpoints declared in `config.ral` — reading each
  `Auth::Env` service's conventional key variable into the in-memory store, then
  removing from the environment *every key variable that was present*, whether or
  not it yielded a usable key. A malformed value (a pasted newline) is still a
  live secret, so it is swept too.
- The scrub makes resolution **eager**: once a variable is gone it cannot be
  re-read, so a key absent at startup stays absent for the run. Read and removal
  happen while the process is still single-threaded — before any session worker —
  so the env mutation cannot race.
- A signed-in **ChatGPT account** never touches the environment: its login is
  loaded from the OAuth token store into its own `OAuth` credential cell
  ([[#Accounts]]). `chatgpt` is a different service from `openai`, so a login
  and an `OPENAI_API_KEY` coexist, and two logins never share a cell.
- This is the subtractive half of exarch's env shaping; the additive half —
  `NO_COLOR`, `$EXARCH_SCRATCH`, and the redirected tool homes — is seeded onto the
  session shell ([[#Bootstrap]]). Per-spawn loader-variable hardening (stripping
  `LD_PRELOAD`/`LD_AUDIT`/`LD_LIBRARY_PATH` under an active grant) is [[map/core|ral-core]]'s,
  not exarch's. The *why* is [[decisions/260613_provider-config-ral-script|provider-config-ral-script]];
  the boundary a child actually inherits is ral's [[design/grant|grant]].

## Bootstrap

`bootstrap.rs` holds the once-per-process pieces; nothing here is per-run.

- **`boot_shell`** — the identity seat's constructor: clear stale ral
  interrupts, install ral's handlers, chain exarch's cancel over them, then
  dress the shell via the shared `exarch_shell` — core's
  [[map/repl/startup|`ral_core::boot::boot_shell`]] with exarch's
  host surface (`builtins::host_surface()`) so the host builtins ride
  construction, the `agent.ral` library, ANSI colour suppressed at the
  source, the exit hints. Its sibling **`engine_boot_shell`** is the wire
  engine's boot recipe (`EngineInstaller::boot`, run engine-side at
  Attach): `exarch_shell` plus an engine-local `Scratch` and
  **`arm_session_ledgers`** — the one policy site arming the binding lease
  and settled-worker retention for both seats — with no signal ceremony
  (a cancel arrives as a `Control` frame) and no terminal probe.
- **Machine probing** — `prompt::host::snapshot` formats the live machine into
  the prompt's `Host` section over core's `ral_core::host` probes (`os`, `now`, `cwd`,
  `user`, `home`, `git`, `exarch logs`), best-effort: a missing value drops its line.
- **`Scratch`** — the disposable per-session directory, exposed under its
  `App`'s own name (`$EXARCH_SCRATCH`; synod's is `$SYNOD_SCRATCH`), with the
  legacy build-tool homes (`CARGO_HOME`, …) redirected
  into it so a write lands inside the grant rather than in a denied real cache.
- **`App`** — the product identity (`EXARCH`; synod names its own) that owns
  the directory conventions as methods: `App::xdg_dir` is the one spelling of
  `$XDG_<kind>_HOME/<app>/` that the project state, model cache, and trusted
  config home all build on ([[design/exarch-config-dir|exarch-config-dir]]);
  `App::project_dir` keys per-project state by a slug of the launch cwd
  (`project_slug`), holding the persisted model selection (`state.json`); and
  `App::log_run_dir` is the durable per-run
  [[map/exarch/frontend|session-log]] directory
  `$XDG_STATE_HOME/<app>/<project>/<run>/`, so logs survive an abnormal exit.

## System prompt

`prompt::assemble` builds an agent-invariant base from `(heading, body)` sections
walked by one uniform renderer, in order **persona, `Ral`, `Editing`, `Builtins`,
`Tasks`, `Script style`, `Host`, [`Workspace`], [`Skills`], [`Surfacing`]**. The
builtin placeholder and the late sections are resolved once per constructed
agent, so a root, identity fork, and wire child each receive their own surface.

- **Persona** (`data/system.md`, unheaded — it sets the tone, not a topic) frames
  the session as *one continuing shell script*: definitions, working directory,
  and worker threads persist across turns, and the working method is act early,
  batch what belongs together, never re-derive.
- **`Ral`** (`data/ral.md`) is the language and tool reference; its handler docs
  follow the lambda-only install rule
  ([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]):
  per-command `handlers:` entries are unary `{ |args| … }`, the catch-all
  `handler:` binary `{ |name args| … }`.
- **`Editing`** documents the file-editing scheme the `--edit` flag selects:
  line-hash (`data/edit-hash.md`) or string-replace (`data/edit-replace.md`);
  only the prompt text switches, both builtins stay registered
  ([[design/hash-addressed-editing|hash-addressed-editing]]).
- **`Builtins`** (`builtin_index`) lists every builtin and prelude function by
  *name only* — a progressive-disclosure index the agent expands at runtime with
  `help`/`explain`, so the prompt cannot drift. `assemble` bakes a
  *placeholder* here: the real per-agent list — filtered to the harness verbs
  that agent holds — is resolved by `BuiltinIndex::apply` once the agent's
  own grants — `returns`, `allow_schedule`, `spawns` (`fuel > 0`) — are in reach,
  without a live `Shell`; zero-fuel agents omit `agent`, `agents`, `message`, and
  `agent-cancel` together.
- **`Tasks`** (`data/tasks.md`) is the task-management kit API.
- **`Script style`** (`data/script-style.md`) is the reuse guide: one program, not
  a nervous probe — define then query, parameterised blocks, records for knobs,
  blocks as policy, long-running work behind `defer`/`await`, and work that must
  outlive the session behind `detach`
  ([[decisions/260725_survives-exit-is-its-own-verb|survives-exit-is-its-own-verb]]).
- **`Host`** is the environment snapshot (`host::snapshot`, [[#Bootstrap]]) with
  the live grant under it: a static legend (`data/grant-legend.md`) over the
  capability bullets, or one ambient-authority line when nothing is attenuated.
- **`Workspace`** (`discover_agents`) collects the `AGENTS.md` instruction files,
  outermost first so the deepest file's recency wins: the operator's
  `<config>/AGENTS.md`, then every repo `AGENTS.md` from the git root down to cwd
  (the walk stops at the first `.git` entry; outside a repo, only `cwd/AGENTS.md`).
  Present whenever any is found; project guidance that cannot widen the grant
  ([[design/agents-md-injection|agents-md-injection]]).
- **`Skills`** lists each discovered readable skill as one `name: description`
  line, loaded on demand with the `skill` builtin — progressive disclosure again.
- **`Surfacing`** (`data/surface.md`) belongs to the interactive base and carries
  the five Bertin marks and role set
  ([[decisions/260619_surface-carries-documents|surface-carries-documents]],
  → [[map/exarch/cards|cards]]). Per-agent resolution then appends
  **`Spawning agents`** (`data/agent-spawn.md`) iff `fuel > 0`, followed by
  **`Agent`** (`data/agent.md`) iff `returns`; returning interactive children
  therefore keep both obligations. A headless root is returning too, so with
  the normal positive spawn fuel it gets both late sections; only a zero-fuel
  returning agent gets `Agent` alone.

`--system FILE...` replaces *only* the persona slot with the user-supplied files;
the per-agent index and optional sections still resolve from the stored base.

## Subsystems

- [[map/exarch/agent|agent]] — the uniform node and the thin `Fleet`: the attend loop
  (provider round-trips, tool-call batches with prompt-queue steering, auto-compaction,
  the nudge-retry policy, sub-agent fork), the `parent` predicate, the owned
  hot-swappable provider, dynamic focus, and the subtree cancel cascade.
- [[map/exarch/provider|provider]] — LLM transport over genai: streaming, the retry
  driver, prompt caching, usage and dollar accounting.
- [[map/exarch/shell-eval|shell-eval]] — one tool call as a ral top-level run under a
  pushed capabilities frame; the streaming digest and the surface host sink.
- [[map/exarch/policy|policy]] — capability composition (base ∨ extend ⊓ restrict) and
  the bake-in profiles; the boundary *is* ral's [[design/grant|grant]].
- [[map/exarch/tools|tools]] — `ral` is the one tool; `tools.rs` is a thin
  seam over it, with no registry. Every other harness verb — the `agent`
  spawn (one record-spec verb, `` `amnemon ``/`` `mnemon `` by field,
  fuel-gated), `reply`, the schedule family — is a builtin reached through
  it, answered by the desk. The sub-agent model is [[design/agents|agents]].
- [[map/exarch/builtins|builtins]] — the resident host atoms and the harness
  verbs: the hash-addressed edit primitives, the spawn/schedule/reply
  family, and the `agent.ral` helpers ([[design/hash-addressed-editing|why]]).
- [[map/exarch/frontend|frontend]] — the agent/UI boundary (event bus, session log) and
  the two frontends, the inline TUI and headless.
- [[map/exarch/cards|cards]] — the render document `surface` carries: a `card` of closed
  Bertin marks decoded once and drawn by one generic interpreter; open card set,
  closed mark set.
- [[map/exarch/io-surface|io-surface]] — every redirect read/write and exec image as one
  card: core emits an I/O event at the runtime doors, exarch binds it to a mark; the
  closed door set is clippy- and meta-test-enforced.

## Sandbox

exarch does not invent its own sandbox. Each tool call is one transport `Run`
carrying the **profile's capabilities in `Run.caps`**, dispatched across the
host seam (`shell_eval::run_shell`, → [[map/exarch/shell-eval|shell-eval]]) and
pushed onto ral's capability stack by core's run door — so the safety
boundary *is* ral's [[design/grant|grant]] mechanism — authority attenuated by
intersection. There is no source-level `grant { … }` the model could escape;
the frame is installed by the host. Profiles ship as `.exarch.ral` files in
`exarch/data/` (`dangerous`, `reasonable`, `edit-only`, `read-only`, `minimal`,
`confined`); see `exarch/PROFILES.md` and [[map/exarch/policy|policy]].

## Scheduled wakeups

With `--allow-schedule`, the agent may schedule its own wakeups (`schedule`,
`schedules`, `unschedule`) — a cron expression or a one-shot `after <dur>`. A
wakeup schedules the *agent*, not a worker: at its time a synthetic user item is
posted to the agent's own inbox and delivered at the exchange boundary, re-engaging
the loop with no human present. It is off by default — waking yourself
indefinitely is real authority. The inbox/reaper mechanics live on the
[[map/exarch/frontend|frontend]] and [[map/exarch/agent|agent]] pages; see
[[decisions/260617_scheduled-wakeups|scheduled-wakeups]].

## Where to look

- `exarch/data/{system.md, ral.md, edit-hash.md, edit-replace.md, tasks.md, script-style.md, grant-legend.md, surface.md, agent.md, agent-spawn.md, agent.ral}` —
  the persona rules, ral reference, editing schemes, task kit, reusable-script
  guide, grant legend, surfacing guidance, returning-agent and spawn contracts,
  and the embedded agent helper library.
- Provider configuration — a famous provider auto-populates from its env key, an
  unusual one from a hand-written XDG `config.ral`
  ([[decisions/260613_provider-config-ral-script|provider-config-ral-script]]).
- `exarch/README.md`, `exarch/PROFILES.md` — human docs.

[[map/repl|repl]] is the sibling `ral` binary over the same engine.
