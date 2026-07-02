---
status: proposed
---

# The async `agent` family could be an exarch host builtin, not a provider tool

**The spawn family (`agent` / `agents` / `agent_cancel`) is a candidate to move
from a provider-advertised [[map/exarch/tools|`Tool`]] to an
[[map/exarch/builtins|exarch host builtin]] reached through the `ral` tool — the
same relocation editing and search already made.** This ADR records the
feasibility of that move, scoped to exarch only, together with the one seam it
requires, an outline plan, and the costs that make it a product choice rather
than a purely mechanical one. It does *not* decide to make the move; it argues
that the move is possible and states what it would take.

The affordance under discussion is the one fixed by
[[decisions/260617_async-agent-tool|async-agent-tool]]: launch-only, always
asynchronous, forks a child from a value-snapshot of the parent shell, runs it on
a detached thread through `Agent::drive`, and delivers the child's single reply
later as an `InboxMsg::AgentResult`. None of that mechanism is in question here.
The question is *where the launch verb lives* — a top-level tool the provider
sees, or a Rust atom the model reaches by writing ral inside the
[[map/exarch/shell-eval|`ral`]] tool.

## Context

Two action surfaces coexist in exarch, and the boundary between them has already
moved once.

A **tool** implements the `Tool` trait in `exarch/src/tools/`: it advertises a
name, description, and JSON schema to the [[map/exarch/provider|provider]], and
its `dispatch` is handed a rich context — `&mut Agent` (the live session),
`&Arc<Provider>`, the tool-call `id`, and the bus [[map/exarch/frontend|`Emitter`]].
`agent`'s `dispatch` uses nearly all of it: it reads `session.caps()` and
`session.cwd()`, computes the child ceiling with `policy::narrow` (`parent ⊓
base`), calls `session.fork(child_caps)`, registers the child in the
session-owned `agents` registry (`session.agents.register(…)`), captures the
parent `session.mailbox()` as the child's upward edge, derives a child
[[map/exarch/frontend|`Emitter`]] via `emit.child(…)` or `emit.muted_child(…)`
depending on `emit.is_session_lived()`, seeds the launch prompt, and spawns a
detached OS thread that drives the child and posts the result on settle.

A **builtin** is registered by `agent_builtins::install_on` *above* ral-core
([[internals/builtins-registry|builtins-registry]];
[[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]) and
reached by the model writing ral. Its body has the core signature
`(&[Value], &mut Shell) -> Settled<Value>` — it sees the
[[map/core/shell-state|`Shell`]] and nothing above it. `grep-files`, `edit`,
`explore-dir`, `fff`, and `skill` all live here, and they are enough of a
precedent that [[map/exarch/tools|tools]] states the principle outright: *editing
and content search are not tools; the model runs them as ral host atoms inside
`ral`.* The spawn family is the obvious next candidate for the same treatment.

The gap that makes the spawn family harder than `edit` is precisely the context
`agent`'s `dispatch` consumes. `edit` and `grep-files` need only the `Shell` —
their file I/O runs below the redirect frame through `shell.check_fs_read` /
`check_fs_write` ([[map/exarch/io-surface|io-surface]]). A spawn needs the
`Agent` (to fork), the `agents` registry, the parent `Mailbox`, the child
`Emitter`, and a `ProviderHandle` — none of which is reachable from `&mut Shell`
today. Bridging that gap is the whole of the work.

**Exarch-only is automatic, not a constraint to enforce.** Exarch atoms are
installed solely by `install_on`; `ral-sh` and any other core embedding never
register `EXARCH_BUILTINS`, exactly as the core-owned `WATCH_BUILTIN` is
installed by the ral host alone and absent from an agent host
([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]). Spawning an LLM
turn is meaningless without a [[map/exarch/provider|provider]], so exarch-only is
also the only coherent scope: there is nothing to suppress elsewhere.

## Feasibility

**Feasible.** The mechanism that lets a host builtin reach heavy host state
through `&mut Shell` already exists, is exercised, and is tested — it is how the
`ral` REPL's editor builtins work. The remaining work is a bounded seam plus a
small refactor of `Agent::fork`, not a redesign. Nor is the reason to hesitate
the prompt-embedding ergonomics once feared — ral's raw strings (below) largely
defuse those. What keeps this a proposal rather than a decision is the loss of
tool-native affordances and the tension with
[[decisions/260617_async-agent-tool|async-agent-tool]]'s stated surface, both
weighed below.

## Why it is feasible

**`BuiltinBody` already admits captured host state.** Beyond
`BuiltinBody::Static(fn(&[Value], &mut Shell) -> Settled<Value>)`, core defines
`BuiltinBody::Captured(CapturedBuiltinFn)` where `CapturedBuiltinFn = Arc<dyn
Fn(&[Value], &mut Shell) -> Settled<Value> + Send + Sync>`, installed per-shell
through `BuiltinTable::install_arc`. This is not hypothetical: `ral/src/repl/
host_handlers.rs` registers the whole `_ed-*` editor family as
`BuiltinBody::Captured(Arc::new(move |args, shell| …))`, closing over REPL host
state and reaching frontend scratch through `shell.repl_mut()` — a
`ReplScratch` that even carries a type-erased `Box<dyn Any>` plugin context. The
`Shell`'s own docstring names the pattern: *exposing it to the host that owns it
is the seam, not a leak.* A spawn builtin is the same shape with a heavier
payload.

**A turn already threads exarch state into the shell.** `run_shell` installs the
call's `Emitter` into the turn as a turn-local `Arc<dyn SurfaceSink>`
(`AgentSink(emit)`), torn down with the turn
([[decisions/260617_turn-local-state|turn-local-state]];
[[decisions/260622_surface-delivers-itself|surface-delivers-itself]]). The seam a
spawn builtin needs is the same kind of turn-local install, carrying more than
the sink.

**The one real wrinkle is a borrow, and it has a clean resolution.**
`Agent::fork` takes `&self`, but a builtin holds `&mut Shell`, which is a reborrow
of a field *inside* that `Agent`; the two cannot coexist, so the builtin cannot
call `fork` as written. The resolution follows from what `fork` actually reads.
Every input to its `Build` is `Clone`, `Arc`, or `Copy` — `system`, the
`ProviderHandle` (`ProviderHandle::new(self.provider.current())`), `focus`,
`interactive`, the `grants_schedule()` bool, the `agents` registry
(`self.agents.clone()`), the parent `id`, and the parent `Mailbox` — and all of
them already cross a thread boundary into the worker today, so all of them are
`Send`. Only the child *shell* comes from the parent shell, and that is
`shell.fork_session()`, reachable from the `&mut Shell` the builtin holds. So
`fork` splits cleanly into a *parent-snapshot* step (run while the `Agent` is in
scope, in `run_shell`) and a *child-assembly* step (`Agent::assemble(Build{…})`,
run inside the builtin from the snapshot plus the freshly forked shell). The
detached worker that runs `Agent::drive` lives on the spawned thread exactly as
today — the builtin only launches it.

## Proposed shape

- **A turn-local spawn seam.** `run_shell` populates, for the extent of the
  turn, a spawn context carrying the parent-snapshot pieces — `Emitter`, parent
  `Mailbox`, `agents` registry, `ProviderHandle`, parent `AgentLog` fork handle,
  `system`, `focus`, `interactive`, schedule grant, parent `id`. It is installed
  and dropped with a guard, the way the surface sink is, so a captured builtin
  closure can never observe a stale context or outlive the turn. It may ride an
  extension of the existing `ReplScratch`-style host scratch or a purpose-built
  slot; either keeps core from inspecting exarch shapes
  ([[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]];
  [[decisions/260628_host-seam-transport-parametric|host-seam-transport-parametric]]).

- **`Agent::fork` decomposes** into `snapshot()` (produces the spawn context,
  `&self`) and child assembly from a snapshot plus a forked shell, leaving the
  existing `fork` as `snapshot()` immediately followed by assembly so current
  callers are unchanged.

- **Three `Captured` builtins** replace the three `Tool` impls, registered in
  `EXARCH_BUILTINS` with type schemes:
  - `agent prompt title permissions` reads the turn's spawn context, computes
    `parent ⊓ resolve_base(permissions)` with `policy::narrow`, forks the child
    shell, assembles the child, registers it, derives the live-or-muted child
    `Emitter`, seeds the prompt, spawns the detached worker (generation and
    `registry.settle` logic verbatim), and **returns a ral record** `{id, title,
    status: "started", log_dir}` — a structured `Value`, not a stringified JSON
    receipt.
  - `agents` returns a list of records `[{id, title, elapsed, log_dir}]` from
    `session.agents.list`.
  - `agent_cancel id` calls `session.agents.cancel` and returns a status value.

- **Registry, inbox, and reaper are untouched.** The child still parents at the
  durable root, the registry still arms `process::reaper` with the
  detached-worker ceiling, `Session::clear` still cancels live workers and
  rejects stale-generation results, and delivery is still an `InboxMsg` rendered
  at the turn boundary. Whether the launch verb is a tool or a builtin changes
  none of this.

- **Surface, prompt, and benchmarks.** `agent`, `agents`, and `agent_cancel`
  leave `registry()` and the `Gate` table in `tools.rs`; the spawn family is no
  longer named in `tools_for`. `exarch/data/system.md` is rewritten to teach the
  builtin form. The terminal-bench and swebench suites are re-run to check
  orchestration behaviour against the current tool baseline.

## Costs

- **Prompt embedding, largely defused by raw strings.** A child `prompt` is
  frequently a long, multi-line block, and as a tool it is a clean JSON string the
  provider validates in isolation. As a builtin it must be embedded in the `ral`
  tool's `cmd` — but ral's hash-bumped raw strings carry it verbatim: `#'…'#` (or
  `##'…'##` when the body itself contains `'#`, a level the lexer and `quote_word`
  select automatically) interprets no `$`, `!`, or `{}`, and passes quotes and
  newlines through literally. The only residual is the outer JSON escaping of the
  `cmd` field, which the model already performs for every multi-line `ral` call —
  so this is not a new hazard, only one more place the existing one applies.
  Because raw strings make a clean embedding *available*, the fear is no longer
  fragility but discipline: whether the model reliably reaches for a raw string
  rather than a bare quote when the prompt carries ral's sigils.

- **A new load-bearing seam.** The spawn context touches the capability,
  cancellation, and generation-safety paths. The precedent (surface sink,
  `ReplScratch`) de-risks the *shape*, but this payload is heavier than either,
  and a turn-local that carried a stale registry or the wrong `Emitter` would be a
  correctness bug in the fleet, not a cosmetic one.

- **Lost tool-native affordances.** A tool gets provider-side schema validation,
  an isolated call block, dedicated permission and UI chrome, and the model's
  strong "one `tool_use` is one action" prior. Folding the verb into ral trades
  all of these for source the model must author correctly.

- **Tension with the originating ADR.** [[decisions/260617_async-agent-tool|async-agent-tool]]
  states "No ral surface" and frames async agent as *an exarch tool mode, not the
  born-durable ral verb*. A host builtin is still exarch-side and still not a
  ral-core language construct, so the "worker body lives below the Provider"
  objection does not bite — but the move plainly cuts against that ADR's letter
  and would need a superseding note there.

- **Prompt and eval churn** as an unavoidable tail.

## Benefits

- **Composability.** As a builtin the launch verb enters ral's value language:
  fan out in a `for` over a task list, bind receipts, filter and sort the
  `agents` listing as ordinary records, gate spawns behind `if`. Today each launch
  is a separate `tool_use` block and orchestration is a sequence of round-trips.
- **A smaller tool schema.** Three tool definitions leave every request's system
  prompt; the model reaches the same authority by writing ral.
- **Structured values.** Receipts and listings become ral records rather than
  stringly-typed JSON the model re-parses.
- **One story.** It extends the existing decision that edit and search are
  builtins rather than tools into a single principle — *the model's action
  surface is ral; the tool set stays minimal* — rather than an exception list.

## Alternatives considered

- **Leave the spawn family as tools.** The status quo, and the safe default: it
  keeps `prompt` a validated string and preserves tool chrome. Chosen against only
  if the composability and schema wins outweigh the embedding risk.
- **Expose the builtins *in addition to* the tools.** Both surfaces at once —
  builtins for programmatic fan-out, the tool for a clean single spawn. Rejected
  as a first step: two surfaces for one affordance doubles the prompt surface and
  the maintenance, and muddies which the model should reach for.
- **A ral-core language verb (a born-durable `Handle` with `poll`/`await`/
  `cancel`).** This is the shape [[decisions/260617_async-agent-tool|async-agent-tool]]
  and [[decisions/260617_long-running-work|long-running-work]] already rejected:
  an LLM turn cannot be a core worker body, and push delivery, not a retained
  handle, is the desired fan-in. Out of scope here — an exarch builtin is above
  core, not in it.

## Open questions

- **Does the model reach for a raw string when the prompt needs one?** ral's
  `#'…'#` raw strings make a correct embedding available and remove the escaping
  hazard; what stays empirical is whether the model reliably *uses* them for a
  prompt carrying `$`, `!`, `{`, or a quote rather than a bare string it then
  mis-escapes. Cheap to probe, and the answer gates nothing structural.
- **Child chrome from a builtin.** The tool emits a `Kind::ToolCall` showing the
  child's prompt and drives the child's tab through `emit.child`. Reproducing the
  same tab-birth and rail rendering from inside a `ral` call — where the rail
  already shows the enclosing `ral` call — needs the child `Emitter` in the spawn
  context and a decision on what the enclosing call renders.
- **Where the seam lives.** Reusing `ReplScratch` versus a purpose-built
  turn-local slot on `Shell` — a naming and layering choice with no correctness
  difference, to be settled against
  [[decisions/260628_host-seam-transport-parametric|host-seam-transport-parametric]].

## See also

[[decisions/260617_async-agent-tool|async-agent-tool]] (the affordance this
relocates, and the "No ral surface" position it revisits),
[[map/exarch/tools|tools]] (the `Tool` trait, `registry`, and the
edit/search-are-builtins principle), [[map/exarch/builtins|builtins]] and
[[internals/builtins-registry|builtins-registry]] (`BuiltinBody::Captured`,
`install_arc`, `EXARCH_BUILTINS`), [[map/exarch/shell-eval|shell-eval]] (`run_shell`,
the turn-local surface sink this seam parallels),
[[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]] and
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (host layers register
above core; the `_ed-*` `Captured` precedent),
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
(the layering the seam must respect), [[map/exarch/agent|agent]] and
[[design/agents|agents]] (`fork`, `assemble`, the tree, `policy::narrow`),
[[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]] (spawning is
universal), and `docs/SPEC.md` §13, §16.
