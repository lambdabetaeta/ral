---
generated_at_commit: 7f817bbe845406e5f1d678c741afcc477e9ef8f2
generated_at_date: 2026-10-28
covers_paths: [exarch/src/main.rs, exarch/src/lib.rs, exarch/src/cli.rs, exarch/src/app.rs, exarch/src/boot.rs, exarch/src/provider/credential.rs, exarch/src/prompt.rs, exarch/src/agent/build.rs, exarch/src/agent/desk.rs, exarch/src/agent/desk/, exarch/data/system.md, exarch/data/agents.md, exarch/data/spawned.md, exarch/data/reply.md, exarch/data/roles.md, exarch/data/ral.md, exarch/data/context.md]
---

# Map: exarch

exarch is a small LLM coding agent — a separate workspace binary that
**embeds** [[map/core|ral-core]] (`ral-core = { path = "../core" }`). A model
(Anthropic / OpenAI / Gemini / xAI / Qwen / OpenRouter / DeepSeek / OpenCode, a
`custom` provider, or an `openai-chatgpt`, `openai-codex` or `xai-grok` account) is
given one `ral` tool, and each call is evaluated as a *ral top-level run*
against a persistent in-process `Shell`, under capabilities the user chose. It
ships as one executable ([[invariants/single-binary|single-binary]]); the same
binary re-execs itself as the [[map/core/capabilities|OS sandbox child]].

The agent is a provider loop over one tool, each run beneath its engine's
host-installed layers — the design argument lives in
[[design/exarch-architecture|exarch-architecture]]. This page maps the
binary's *entry point*: what runs at startup, and how the system prompt is
assembled.

## Entry and dispatch

`run` (in `lib.rs`, lifted out of `main` so integration tests link the whole
crate) is the session path; `main.rs` is the thin shell over it. On the way out,
`main` runs `ral_core::authority::sandbox::teardown_session` — deleting the session's
AppContainer profiles on Windows, a no-op elsewhere — before surrendering the
exit code.

- **Pre-`main` trampoline.** Before any setup, `exit_if_re_exec_child` serves
  and exits a re-exec child: it hands the role
  `ral_core::classify(argv)` names to core's one pre-`main` dispatch,
  `ral_core::invocation::serve_process` ([[map/repl/startup|startup]]), over
  exarch's `INSTALLERS` (whose `narrow` is `policy::base_layer`, since core
  carries no base-tag lexicon of its own and a wire-seeded child resolves its own
  layer while booting). A confined child is served before any pin, the anchor
  unpinned; every other role — the `--engine` engine child
  included — is pinned by [[map/core/capabilities|`authority::sandbox::boot`]] before it runs.
  `main` and **every test `#[ctor]` (`pre_main_ctor!`) share this dispatch and
  its exit** — they differ only in where argv comes from: a constructor calls
  `serve_process_before_main`, because on musl std has no argv before `main`
  and only `/proc/self/cmdline` does. That names the role but not the process
  it runs in: std's argv stays empty, and uucore reads it, so no constructor on
  musl serves a bundled tool. Tests that need one are ignored under
  `cfg(ctor_without_argv)`, which core's and exarch's build scripts set on Linux
  without glibc. Process-control fixtures use `test_helper::SLEEP`, a host
  program by path (`/bin/sleep`) on Unix and the bundled tool elsewhere, so
  cancellation tests do not depend on musl's pre-main argv. A test binary reaches
  `main` only through libtest yet is the same
  [[invariants/single-binary|multicall executable]] a child re-execs; skip the
  dispatch and the confined transport stays unpinned, so that binary's
  confined-path tests cannot run.
- **Subcommands** (`cli.rs`) run an out-of-band action and exit before any session
  setup: `login` / `logout` / `accounts` manage signed-in accounts (see
  below), `--model` and the session flags are ignored on this path. `export
  <session> [--out FILE]` renders a session's `record.jsonl` (given as its
  directory or the file) as the plain-text transcript the scrollback showed, to
  standard output or a new file, through the same `tui::export_to` that `/export`
  uses ([[internals/session-record|session-record]]).
- **Session.** Absent a subcommand, `run` settles what to open on
  (`--provider` pins the identity; `--model` names a model its account must
  list; `--via` names the upstreams that serve it on a routing service;
  `--effort` names a rung of the `/model` picker's own `EFFORT_LADDER`;
  `--chat` drops the system prompt and all tools). The saved project selection
  is the ground each of those is laid over. A choice the flags name must build or
  the launch fails; a saved choice that no longer builds, or no choice at all,
  opens the picker alone on the screen before any session exists (`tui::choose`)
  — or, headless, fails asking for `--model`. A session is therefore born with a
  listed model and never holds none, and the choice it opens on is saved back, so
  a flag and the picker leave the same trace. `--role NAME=MODEL` (with
  `--role-provider NAME=ACCOUNT` to break a tie, and `--role-via NAME=TAGS` to
  choose its upstreams) binds the models a spawned
  agent may run on, minted at launch with this run's `--effort` and
  `--max-tokens` and never saved; without roles every child inherits
  ([[map/exarch/provider|provider]]). Then `run` composes the capability lattice
  (`policy::for_invocation`, passing `sandbox::pinned_arg0()` for its self layer;
  → [[map/exarch/policy|policy]]),
  assembles the system prompt (`prompt::assemble`), builds the first root
  [[map/exarch/agent|`Agent`]] via `Agent::root(RootConfig, RootEngine, provider)`
  — or, for the TUI under `--resume`, every live root of the run via
  `Agent::resume` — the exarch binary uses `RootEngine::Identity`; a wire
  engine runs remotely, and synod reuses the same construction — and hands off to one frontend — the inline TUI or the
  pipe-friendly headless runner, selected explicitly by `--headless` or
  implicitly by `--prompt` (which refuses to resume a run with more than one
  open session) — which shares the roots' one [[map/exarch/agent|`Fleet`]].

## Accounts

**Each login service may own many accounts.** `openai-chatgpt` and `xai-grok`
key by returned subject, not email; independently selectable Chatgpt workspaces
are not promised unless returned subjects distinguish them. `openai-codex`
keys by subject and backend account/workspace id together. Every account is
independently selectable in `/model`, with its own credential cell and catalog.
The shape is [[map/exarch/provider#Services and accounts|`Service` × `Account`]].

- `exarch login openai-chatgpt` adds a browser token-sharing account.
  `exarch login openai-codex` adds a Codex browser account;
  `exarch login openai-codex --device-auth` uses its device method.
  `exarch login xai-grok` uses Grok device authorization. No aliases exist.
- `exarch login openai-chatgpt <account>` renews an existing Chatgpt account;
  the same service-first grammar applies to Codex/Grok. A service mismatch
  or different returned identity is refused rather than replacing credentials.
- `/login` offers accounts and the three services, then only supported methods;
  successful admission updates the live credential store and catalog.
- `logout` removes one account by handle or id, or `--all`. Ambiguous handles
  are refused with candidate ids. Codex removal is local-only; Chatgpt/Grok
  attempt issuer revocation, without promising immediate token invalidation.
- `accounts` labels the signed-in set with `identity::label` against its peers.

The store and all caches key on full service-qualified `AccountId`. Login
accounts follow key-bearing accounts, ordered by login declaration order,
handle and id. No signed-out login gets a synthetic account. Old `chatgpt:*`
entries are rejected with warnings, never migrated; stale cache keys do not
alias new accounts and stale selection follows the normal unavailable/picker
path. See [[decisions/261008_sign-in-with-chatgpt-token-sharing|sign-in-with-chatgpt-token-sharing]]
for public-client policy caveats and deferred live acceptance.

## Credentials and the env scrub

`provider/credential.rs` resolves every provider's secret once at startup and **scrubs
the key variables from the process environment, so no child a tool call spawns
can inherit a live key.**

- **`CredentialStore::resolve_and_scrub`** sweeps every known account — the
  built-in table and the endpoints declared in `config.ral` — reading each
  `Auth::Key` service's key variable into the store's environment layer, then
  removing from the environment *every key variable that was present*, whether or
  not it yielded a usable key. A malformed value (a pasted newline) is still a
  live secret, so it is swept too.
- Removal is **blank, then unset**: `unsetenv` drops only libc's pointer, while
  `/proc/<pid>/environ` reads the bytes the kernel laid down at `execve`, so each
  value is overwritten in place first, and the process is made non-dumpable
  before any of it ([[invariants/a-grant-never-reaches-its-own-key|a-grant-never-reaches-its-own-key]]).
- The scrub makes resolution **eager**: once a variable is gone it cannot be
  re-read, so a key absent at startup stays absent for the run. Read and removal
  happen while the process is still single-threaded — before any session worker —
  so the env mutation cannot race.
- A **signed-in account** never touches the environment: each login is loaded
  from the OAuth token store into its own credential cell ([[#Accounts]]).
  The three login services are distinct from `openai` and `xai`, so API keys
  and logins coexist without sharing credentials.
- This is the subtractive half of exarch's env shaping; the additive half —
  `NO_COLOR`, `$EXARCH_SCRATCH`, and the redirected tool homes — is seeded onto the
  session shell ([[#Bootstrap]]). Per-spawn loader-variable hardening (stripping
  `LD_PRELOAD`/`LD_AUDIT`/`LD_LIBRARY_PATH` under an active grant) is [[map/core|ral-core]]'s,
  not exarch's. The *why* is [[decisions/260613_provider-config-ral-script|provider-config-ral-script]];
  the boundary a child actually inherits is ral's [[design/grant|grant]].

## Bootstrap

Two modules hold the once-per-process pieces; nothing here is per-run.
`app.rs` is the apps and their directories (`App`, `EXARCH`, `SYNOD`, the XDG
and project dirs, `Scratch`, `RunLock`, the resume list, the time and slug helpers); `boot.rs` is
the shell every engine starts from.

- **`engine_runtime` and `engine_root`** (`boot.rs`) — the installer's two
  halves for both carriers, run engine-side at Attach: `engine_runtime` is core's
  [[map/repl/startup|`ral_core::shell::boot::boot_runtime`]] with exarch's host surface
  (`builtins::host_surface()`) so the host builtins ride construction, and a
  child engine runs it too; `engine_root` is the root-only recipe over a root
  session: `library::install_agent_library` for the `agent.ral` helpers, ANSI colour
  suppressed at the source (`seed_no_color`), the scratch the `Attach` names (it refuses one that names none: the host owns it), and
  **`shell_eval::arm_session_ledgers`** — the one policy site arming the
  binding lease and settled-worker retention. It has no signal ceremony and no
  terminal probe (a cancel arrives as a `Control` frame).
- **`face_process_signals`** (`boot.rs`) — the signal ceremony, once at each
  entry point that hosts an identity engine: clear stale ral interrupts,
  install ral's handlers, chain exarch's cancel over them
  (`signals::install`), seat the terminal.
- **Machine probing** — `prompt::host::snapshot` formats the live machine into
  the prompt's `Host` section: `now` (via `jiff`) and `git` (via `git(1)`) are probed
  there, `cwd`, `user` and `home` come from `ral_core::host`; best-effort, a missing
  value drops its line.
- **`Scratch`** — the disposable per-session directory, exposed under its
  shared `app::SCRATCH_VAR` (`EXARCH_SCRATCH`) name. It is
  an `OwnDir` under `$XDG_CACHE_HOME/<app>/scratch/`, on disk and beside no
  one else's files, never in the temp dir, with a lock file named for it
  that the session holds for as long as it runs. Every cache a session
  writes is redirected into it: `TOOL_HOMES`, one table of variables, each
  replacing the inherited value with a subdirectory of the scratch
  (`TMPDIR`, `XDG_CACHE_HOME`, `CARGO_HOME`, `GOPATH`, `GOCACHE`, the pnpm, Bun and
  Yarn stores, …) or, for Maven, which has no such variable, `MAVEN_ARGS`
  set to `-Dmaven.repo.local=<scratch>/maven`. The table is spelled in two
  namespaces by one `env_under`: a host `Scratch` joins its homes natively
  (`join_slash`), and `guest_scratch_env`, a function of the app and a
  `GuestPath` alone, which synod's guest needs, joins with `/` on every host
  (`GuestPath::join`), since a guest path lifted into a host `PathBuf` would
  join `/tmp\pip` on Windows. `Engine::root` attaches its identity engine at
  `EnginePath::Host` paths, the launching user's cwd and `HOME`, with the
  launch's grants in `Root::grants`. The five restrictive profiles write no user caches, so
  a write lands in the scratch rather than in a denied real one. The
  five restrictive profiles grant it as `` `scratch . ``; they cannot spell
  it `xdg-cache`, since the session rebinds `XDG_CACHE_HOME`. `TMPDIR` points
  to `<scratch>/tmp`, created by `Scratch::made` on the host and after the
  guest daemon mounts its scratch tmpfs. The profiles grant neither `/tmp`
  nor `tempdir`; `dangerous` leaves fs unrestricted. Hardcoded `/tmp` is
  per-command on Linux and refused on macOS. The scratch is made before the
  policy freezes, and
  travels as a spelling: `Scratch::spelling` is the `$EXARCH_SCRATCH` of the
  root's `Attach`, which `engine_root` hands to the shell
  (`Shell::set_scratch`), so that every grant frozen in the session, a
  model's nested `grant` included, anchors a `scratch` place to it.
  `RUSTUP_HOME` is not in the table: it holds what runs and stays the
  host's. A `Hold::Session`'s drop deletes the directory, then closes and
  removes its lock; `reap_unheld`, run by `Scratch::new`, deletes the
  scratches no live session's lock answers for, which covers a session
  killed before its drop runs. Beside the table is the one setting the
  confinement forces without relocating anything
  (`CARGO_NET_GIT_FETCH_WITH_CLI`, because cargo's bundled TLS reaches the
  keychain service the profile withholds — see
  [[map/core/capabilities|capabilities]]).
- **`App`** — the product identity that owns the directory conventions as
  methods, and the engine names every product itself (`EXARCH`, `SYNOD`, over
  `APPS`) rather than each naming its own: composing a grant means carving out
  every product's credential directories, so one place has to know which apps
  there are. `App::xdg_dir` is the one spelling, as a name, of
  `$XDG_<kind>_HOME/<app>/` (core's `path::app_dir`, which ral's own directories
  share), for the consumers that take a name: a credential deny, a skill root,
  the `AGENTS.md` chain; it is `None` where the host names no base.
  `App::own(kind)` is the same directory *held* (`OwnDir::xdg`), which
  everything that writes goes through: the project state, the model cache, the
  token store, and the trusted config home, whose `config.ral` is a `UserFile`
  ([[design/exarch-config-dir|exarch-config-dir]]);
  `App::project` keys per-project state by a slug of the launch cwd
  (`ral_core::path::project_slug`: a readable part, then 16 hex digits of the
  SHA-256 of the cwd's exact OS bytes, so two cwds that differ at all key
  apart), holding the persisted model selection (`state.json`); and
  `App::new_run` makes the durable per-run
  [[map/exarch/frontend|session-log]] directory
  `$XDG_STATE_HOME/<app>/<project>/<run>/`, `<run>` being
  `<YYYY-MM-DD-HHMMSS>-<pid>`, so logs survive an abnormal exit and no other
  live exarch can choose the name; each session's `sessions/<id>/` holds
  `record.jsonl` alone. `RunLock` is the run's advisory lock, `run.lock`, held
  with `File::try_lock` for the process's life. None of these falls back to the
  temp dir where the host has no base: the account's home stands in, and past
  that `OwnError::NoBase` says what to set ([[decisions/261022_own-files|own-files]]).

## System prompt

`prompt::assemble` builds an agent-invariant base from `(heading, body)` sections
walked by one uniform renderer, in order **persona, `Ral`, `Editing`, `Builtins`,
`Tasks`, `Context management`, `Host`, [`Workspace`], [`Skills`],
[`Surfacing`]**. The builtin placeholder and the late sections are resolved
once per constructed
agent, so a root, identity fork, and wire child each receive their own surface.

- **Persona** (`data/system.md`, unheaded — it sets the tone, not a topic) frames
  the session as *one continuing shell script*: definitions, working directory,
  and worker threads persist across turns. It teaches selective binding, bounded
  inspection, batching known steps, and spawning independent long-running
  work under a title, binding its handle, and `await`ing that handle when the result is needed.
- **`Ral`** (`data/ral.md`) is the language and tool reference. The CBPV
  introduction leads into values, computations, capture, and forcing, followed
  by help and exploration. String arguments remain intact; list spreading and
  glob expansion are explicit. Blocks retain their lexical bindings when a
  helper is shadowed. Examples collect multiple reads into one returned value
  and force computations inside homogeneous lists and heterogeneous records;
  string guidance distinguishes variable delimiters from command capture and
  teaches hash-delimited raw strings as the spelling for literal text.
  Specialist details live in `explain redirects`,
  `explain within`, `explain return`, `explain grant`, `explain try`,
  `explain await`, `explain poll`, and `explain detach`; the prompt keeps
  common forms and examples.
  Failure guidance separates recovery, inspection, and deliberate suppression.
  The concurrency section teaches one pattern: `let h = spawn "title" { … }`, other
  work or the end of the turn, a host notice when the worker ends, then
  `await`, which behaves as the block run on that line — its output written
  there, its value returned, its failure raised, so `try` recovers it — and
  claims the result once, so a second `await` fails. `workers` lists every
  worker (`running`, `stopping`, or `ready`), `poll` and `is-done` are total
  four-state probes over `running`, `stopping`, `ready`, and `consumed` that
  never wait or claim, a block's own spawns finish before it does, and
  fan-out that must leave nothing behind is a `gather { … }`, with `par`,
  `both`, `first-of` over it and `race` for handles already held; a `gather`
  is synchronous, so work that should outlive the turn is a loose `spawn`
  outside it. A loose worker lives as long as the session; `detach` is for a process that must outlive
  it.
  The audit example distinguishes an expected nonzero exit from an error;
  independent calls use individual `attempt`s so one failure does not skip the
  rest, then inspect each command's status rather than the enclosing audit's
  success. Exarch includes this reference directly; synod appends its own
  office examples ([[map/synod|synod]]).
- **`Editing`** documents the file-editing scheme the `--edit` flag selects:
  line-hash over `view-hash` (`data/edit-hash.md`, `--edit hash`) or
  string-replace over `view-text` (`data/edit-replace.md`, `--edit replace`); only the
  prompt text switches, every reader and editor stays registered
  ([[design/hash-addressed-editing|hash-addressed-editing]]).
- **`Builtins`** (`builtin_index`) lists every builtin and prelude function by
  *name only* — a progressive-disclosure index the agent expands at runtime with
  `help`/`explain`, so the prompt cannot drift. `assemble` bakes a
  *placeholder* here: the real per-agent list — filtered to the harness verbs
  that agent holds — is resolved by `BuiltinIndex::apply` (`agent/index.rs`)
  once the agent's own grants — its `Role`, `allow_schedule`, `spawns`
  (`fuel > 0`) — are in reach,
  without a live `Shell`; zero-fuel agents omit `agents` alone.
- **`Tasks`** (`data/tasks.md`) is the task-management kit API.
- **`Context management`** (`data/context.md`) teaches the coding workflow:
  evict irrelevant retrievals and failed attempts once retried, each as one
  run `[from, to]` under a required note that leaves one residue; cut at the
  bottom or in batch, covering a residue whole or not at all; expect the
  a forced eviction to take the current prompt too, so evict first under
  one's own note, saying what was asked; and find residues again with
  `exarch-transcript`. Each instruction is paired with
  an example call; the operation-by-operation `explain` guides in
  `shell_eval/builtins/harness.rs` own returned shapes, limits, refusals, and
  recovery details. Eviction changes visibility, never filesystem or shell
  state.
- **`Host`** opens by naming the reader — the one host fact that is not the
  host's, since a name is what every other agent addresses it by and the
  only way it picks its own row out of a roster — then gives the environment
  snapshot (`host::snapshot`, [[#Bootstrap]]) with the live grant under it:
  a static legend (`data/grant-legend.md`) over the capability bullets, or
  one ambient-authority line when nothing is attenuated. The name is a
  second placeholder beside the builtin index (`NAME_PLACEHOLDER`), filled
  per agent by `BuiltinIndex::apply`, because the section is otherwise baked
  once at boot. The grant is a third (`GRANT_PLACEHOLDER`), filled per agent
  by `prompt::with_grant` from its own engine's `Probe::Grants` alone: a
  child's layer is resolved in its own engine (`Shell::child_of`), so only
  that engine can name it. `grant_summary` renders every layer in order.
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
  **`Agents`** (`data/agents.md`) iff `fuel > 0`, then **`Spawned`**
  (`data/spawned.md`) iff the agent is a child, then **`Reply`**
  (`data/reply.md`) iff it returns at all. `Reply` says only what is true of
  any receiver; `Spawned` carries the spawner's snapshot, its binding the
  reply, and the idle wait for follow-ups. A headless root therefore gets
  `Reply` but never `Spawned`. `Agents` uses point/call examples: pass bindings,
  request a reply shape, assign disjoint write grants, wait without polling,
  read, follow up, and cancel. **`Roles`** (`data/roles.md` plus one
  `name: model` line per `--role`) follows `Agents`, iff the agent spawns and
  the human bound any.
  The detailed permissions, schemas, and fan-out examples live in
  `explain exarch-agents`.

The assembled prompt is a fact of the session: every opening of a log (a root,
a fork, a `/new` or `/branch` root) records it as `Protocol::System`, and a
resume records the freshly built prompt only if it differs from the one in
force. A round-trip reads it off the model fold, so the owner and its address hold no prompt copy.

`--system FILE...` replaces *only* the persona slot with the user-supplied files;
the per-agent index and optional sections still resolve from the stored base.

## Subsystems

- [[map/exarch/agent|agent]] — the uniform node and the thin `Fleet`: the attend loop
  (provider round-trips, tool-call batches with prompt-queue steering, context eviction,
  the nudge-retry policy, sub-agent fork), the `Role` each node is born with, the owned
  hot-swappable provider, dynamic focus, and the life/work/exchange scope tree.
- [[map/exarch/provider|provider]] — LLM transport over genai: streaming, the retry
  driver, prompt caching, usage and dollar accounting.
- [[map/exarch/shell-eval|shell-eval]] — one tool call as a ral top-level
  run under its engine's host-installed session layers; the streaming digest
  and the surface host sink.
- [[map/exarch/policy|policy]] — capability composition (base ∨ extend ⊓ restrict) and
  the bake-in profiles; the boundary *is* ral's [[design/grant|grant]].
- [[map/exarch/tools|tools]] — `ral` is the tool, and `thinking` a hidden-flag
  relay beside it; `provider/tools.rs` holds the wire half (a static `ToolSpec` and a
  `Copy` `Toolset` the agent carries, with no registry) and `agent/tools.rs` the dispatch. Every other harness verb — the
  `` exarch-agents `start `` spawn (one record-spec tag, `` `amnemon ``/`` `mnemon ``
  by field, fuel-gated, `` `reply ``/`` `read `` gated on a returning `Role`), the schedule family — is a builtin reached
  through it, answered by the desk. The sub-agent model is [[design/agents|agents]].
- [[map/exarch/builtins|builtins]] — the resident host atoms and the harness
  verbs: the hash-addressed edit primitives, the spawn/schedule/reply
  family, and the `agent.ral` helpers ([[design/hash-addressed-editing|why]]).
- [[map/exarch/frontend|frontend]] — the agent/UI boundary (event bus, session log) and
  the two frontends, the inline TUI and headless.
- [[map/exarch/cards|cards]] — the render document `surface` carries: a `card` of closed
  Bertin marks decoded once and drawn by one generic interpreter; open card set,
  closed mark set.
- [[map/exarch/io-surface|io-surface]] — every redirect read/write and exec image as one
  card: core emits an I/O event at the runtime sites, exarch binds it to a mark; every
  exempt site is a typed `site!`, enforced by clippy and by a meta-test
  ([[decisions/261022_typed-sites|typed-sites]]).

## Sandbox

exarch does not invent its own sandbox. The composed profile layers ride the
root's `Attach` as `Root::grants` and are pushed once, as permanent session
layers, when the engine boots, after its recipe and env. Every call, probe
and child runs beneath them. Each tool call is one transport `Run`,
dispatched across the engine protocol (`shell_eval::run_shell`, →
[[map/exarch/shell-eval|shell-eval]]). The safety boundary *is* ral's
[[design/grant|grant]] mechanism — authority attenuated by intersection. The
host installs that frame, so no source-level `grant` can escape it. Profiles
ship as `.exarch.ral` files in `exarch/data/` (`dangerous`, `reasonable`,
`edit-only`, `read-only`, `minimal`, `confined`); see `exarch/PROFILES.md`
and [[map/exarch/policy|policy]].

## Scheduled wakeups

With `--allow-schedule`, the agent may schedule its own wakeups (`schedules`)
— a cron expression or a one-shot `after <dur>`. A wakeup schedules the
*agent*, not a worker: at its time a synthetic user item is posted to the
agent's own inbox and delivered at a tool or exchange boundary, re-engaging
its loop with no human present. Each schedule holds a weak mailbox for the
work in which it was armed: abandoning that work makes its wakeup stale;
`reply` and `/clear` also clear the registry. Stop leaves retirement to the
callback, and Synod refuses scheduling outright. It is off by default — waking yourself
indefinitely is real authority. The inbox/reaper mechanics live on the
[[map/exarch/frontend|frontend]] and [[map/exarch/agent|agent]] pages; see
[[decisions/260617_scheduled-wakeups|scheduled-wakeups]].

## Where to look

- `exarch/data/{system.md, ral.md, edit-hash.md, edit-replace.md, tasks.md, context.md, grant-legend.md, surface.md, agents.md, reply.md, agent.ral}` —
  the persona rules, ral reference, editing schemes, task kit,
  context discipline, grant legend, surfacing guidance, returning-agent
  and spawn contracts,
  and the embedded agent helper library.
- Provider configuration — a famous provider auto-populates from its env key, an
  unusual one from a hand-written XDG `config.ral`
  ([[decisions/260613_provider-config-ral-script|provider-config-ral-script]]).
- `exarch/README.md`, `exarch/PROFILES.md` — human docs.

[[map/repl|repl]] is the sibling `ral` binary over the same engine.
