---
status: active
---

# Uniform agent nodes: the fleet, the tree, and the parent-less trunk

**An exarch run is a *fleet* of structurally identical *agents* arranged in a
tree, not a privileged root that owns some sub-agents.** Today one `Session`
conflates two things — *the run* (a bus, a registry of live workers, a lifetime)
and *one agent* (a shell, a context, an inbox, a tool set). This decision splits
them: a thin **`Fleet`** holds the registry and the one event bus; an **`Agent`**
is the uniform node. Every agent may spawn, so the spawn tree is unbounded in
depth rather than capped at one level. The *sole* distinguished node is the one
with **no parent** — the *trunk* — and even its two residual behaviours
(withholding `reply`, parking unconditionally) follow *derivationally* from
"has no parent and a human converses with it," never from an `is_root` branch.
The human is *attached* to exactly one agent at a time — the *focused* node — and
attachment is dynamic: `TAB` moves it, the focused agent receives the human's
turns and `Esc`, and a `reply` still terminates an agent even mid-conversation.

This **refines** [[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]
(its reply gate `is_root && interactive` becomes `parent = None ∧ interactive`,
the same fact read through tree position) and **supersedes** the depth-1
restriction and the `spawns()` tool-set axis of
[[decisions/260617_async-agent-tool|async-agent-tool]] and
[[design/agents|agents]]. It builds on
[[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]] (the
bus already outlives the turn) and generalises
[[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]] from one
root-turn token to a per-focus token that cancels the focused agent *and its
subtree*.

## Context

The structure was proposed in conversation: *a session holds agents, no agent
distinguished as root, dying when no active agents remain; a common bus all
agents write; an agent holding its context, a hot-swappable provider, a message
queue, tools, a nudger; the TUI a view into the bus, beholden to nobody.* Most
of it already exists, under names that hide it:

- **The agent already exists — it is called `Session`** (`session.rs:35`): one
  shell, one `SessionLog` (model view), one transcript, one `Inbox`, one
  `nudge::Registry`, one `cancel::Token`, one `ToolSet`, plus an
  `agents: AgentRegistry` of async children.
- **The event bus already exists and is fan-out**: `SessionBus`/`Emitter` stamp
  every `Kind` with a `SessionId`; the TUI is a `Sink` that slices by id into
  tabs ([[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]).
  It is *not* the inbox: a child's result routes point-to-point to *its parent's*
  inbox (`tools/agent.rs:256`), never a sibling's.
- **The fleet is implicit**: the root `Session` owns the registry, and `tui::run`
  separately owns the bus and a clone of that registry (`tui.rs:2738`,
  `tui.rs:2778`). No object names "the run."
- **The roles already decompose** into two `ToolSet` booleans — `spawns()` and
  `replies()` ([[design/agents|agents]]). The three roles (interactive root,
  headless root, peer) are points in that two-axis space, and `is_root` +
  `park_when_idle` are two spellings of one fact (`session.rs:317`).

So the request is largely a **re-cut of existing seams**, with three genuine
behavioural changes: drop the depth-1 spawn cap, make the provider per-agent,
and make human attachment *dynamic*. The design question those force —
**tree-with-uniform-nodes vs flat-set** — is settled in favour of the tree:
result routing to a parent (`InboxMsg::AgentResult`) *is* a parent edge, so
"no agent distinguished" cannot mean a flat set. It means **no agent is
privileged by special-case code** — only by its position (whether `parent` is
`Some`).

## Decision

### The `Fleet` / `Agent` split

- **`Agent`** (was `Session`; `git mv session.rs → agent.rs`) is the uniform
  node, holding everything a `Session` holds today plus a `parent: Option<AgentId>`
  and an owned `ProviderHandle`. `SessionLog → AgentLog`, `SessionId → AgentId`
  (already aliased, `bus.rs:26`), `SessionBus → FleetBus`.
- **`Fleet`** is the new thin object: `{ agents: AgentRegistry, bus: FleetBus,
  focus: AgentId, interactive: bool }`. The registry is now the *fleet's* — every
  agent registers itself, not only a root's async children — so "all live agents"
  is literally the registry's contents. `tui::run` / `headless::run` build the
  `Fleet` and read bus, registry, and focus from it.
- **The TUI is unchanged in spirit** — it already slices the bus by id and looks
  peers up in the registry; it now reads both from one object and owns the
  `focus` it mutates on `TAB`.

### The trunk is the parent-less node — and that single fact carries every asymmetry

There is no `is_root`. The distinctions reduce to one structural predicate and
the fleet's `interactive`/`focus`:

```
  trunk(a)       ⟺  a.parent = None
  conversing(a)  ⟺  trunk(a) ∧ fleet.interactive          // the sole distinguished agent
  replies(a)     ⟺  ¬ conversing(a)                       // everyone but the interactive trunk has `reply`
  park_idle(a)   ⟺  a.schedules.armed()
                  ∨  fleet.focus = a.id
                  ∨  conversing(a)
```

- **`replies()` becomes `¬conversing(a)`.** The interactive trunk converses
  across turns and has nowhere to return, so it withholds `reply`; *every other
  agent* — a peer at any depth, *and* a headless trunk (`parent = None`,
  `interactive = false`) which is seeded once and returns one result — advertises
  it. This is exactly
  [[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]'s
  `is_root && interactive`, with `is_root` re-read as `parent = None`.
- **Parking is computed, not stored.** `is_root` and `park_when_idle`
  (`session.rs:57,62`) are deleted. The interactive trunk parks because it is
  `conversing`; a focused sub-agent parks because the human is talking to it; an
  agent with a live self-schedule parks because it armed one
  ([[decisions/260617_scheduled-wakeups|scheduled-wakeups]]). A headless trunk
  satisfies none of the three, so it terminates at quiescence — the one-shot
  contract, unchanged.

### Uniform spawning: the tree is unbounded in depth

- **`ToolSet::peer()` gains the spawn family**; the `spawns()` axis becomes
  universally true and is **deleted** from `ToolSet` and the `allows` check
  (`tools.rs`). The constructors reduce from three to two: a `conversing` set
  (no `reply`) and a `returning` set (with `reply`) — both spawn.
- **Depth-N already works structurally**: a child registers in its parent's
  registry (now the fleet's), and `fork` already snapshots the parent's shell by
  value (`session.rs:359`, `Shell::fork_session`) at any depth. Only the withheld
  tool capped it. The `agent` tool description drops "A sub-agent cannot itself
  spawn sub-agents" (`tools/agent.rs:93`).

### Cancellation cascades the subtree

- `AgentRegistry::Entry` (`agent_registry.rs:61`) gains a parent link (or
  child-set). `cancel`, the ceiling reaper, and `/clear` **walk descendants**, so
  cancelling a mid-tree agent reaps its whole subtree. This single cascade serves
  three callers: `agent_cancel`, the per-agent ceiling, and `Esc`.
- **`Esc` targets `fleet.focus`'s turn and its subtree** (not "the root"). This
  generalises [[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]: the
  focused agent re-mints its published token per turn; the cascade carries the
  cancel down. Generation rejection on `/clear` (`agent_registry.rs:145`) is
  unchanged — a late result from a cleared generation is still dropped.

### The provider is per-agent and hot-swappable

- `ProviderHandle` (`session.rs:181`) moves from a `drive()` parameter onto the
  `Agent`. `drive` reads `self.provider`. `/model` swaps the **focused** agent's
  handle; a swap on one agent never disturbs another (today's peer snapshot at
  `tools/agent.rs:237` becomes the child owning its own handle, seeded from
  `parent.provider.current()` at `fork`).

### Focus is the dynamic attachment

- The `Fleet` owns one `focus: AgentId`. `TAB` moves it; the focused agent
  receives the human's typed lines as fresh `Turn`-boundary turns and owns `Esc`.
- **Parking is re-evaluated, because focus changes while an agent is parked.**
  `Inbox::next_or_idle(park_when_idle: bool, …)` becomes
  `next_or_idle(should_park: impl Fn() -> bool, …)`, the predicate re-checked on
  every `Condvar` wake (`bus.rs:16`). On `TAB`, the UI thread stores the new focus
  and notifies the *previous* and *new* focused inboxes; the de-focused agent
  wakes, finds its inbox empty and `should_park()` now false, and terminates at
  quiescence (unless trunk or scheduled).
- **`reply` ends a focused agent.** A returning agent's `reply` terminates it and
  returns its value to its parent regardless of focus; the conversation ends out
  from under the human. The TUI then **falls focus back to the parent**, recursing
  to the trunk.

### Liveness: the fleet dies when the registry empties

- **`fleet.alive() ⟺ registry non-empty`** — the literal reading of the original
  "dies when no active agents remain." An agent removes itself at termination
  (`reply`, quiescence, cancel). The interactive trunk stays in the registry until
  `/quit`, because it parks; a headless trunk leaves at quiescence; a sub-agent
  leaves on settle.
- **No daemon.** Nothing remains in the registry without a *present human* (the
  conversing trunk, or a focused agent), *running work*, or a *bounded
  self-schedule*. There is no headless, human-less fleet that lives indefinitely
  on detached work — that mode is rejected.

## Why this shape

- **One distinction, read through position.** Collapsing `is_root` +
  `park_when_idle` + the `replies()` axis into `parent: Option<AgentId>` makes the
  trunk's asymmetry *underivable from a special case* — it falls out of having no
  parent. This is the honest content of "no agent is distinguished": the tree
  still has a root, but the code has no root-branch.
- **The two buses stay separate, as they must.** The event bus is fan-out
  (broadcast, sliced by id); the inbox is point-to-point (a result to *its*
  parent, with a per-message drain `Boundary`). Merging them would lose result
  routing and the mid-turn-vs-fresh-turn distinction
  ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]],
  [[decisions/260617_scheduled-wakeups|scheduled-wakeups]]). The fleet owns the
  bus; each agent owns its inbox.
- **The tree is load-bearing, not incidental.** Result routing to a parent *is* a
  parent edge; "flat set of peers on a bus" cannot express it. Uniform nodes keep
  the tree and merely stop privileging its root.
- **It reuses what exists.** The shared `drive`/`apply` loop is already "root and
  peer alike" (`session.rs:481`); it loses two parameters and gains two field
  reads. The bus, the registry, `fork`'s value snapshot, generation rejection, and
  the ceiling reaper are all retained. The genuinely new code is small: the
  **cancellation cascade** and the **focus re-check**.

## Realisation

Six phases, each compiling green and reviewable alone; the heavy diff (Phase 1)
carries no behaviour.

1. **Rename, no logic change.** `git mv session.rs → agent.rs`; `Session →
   Agent`, `SessionLog → AgentLog`, `SessionBus → FleetBus`; canonicalise
   `AgentId`. `cargo test` green with zero behavioural diff.
2. **Introduce `Fleet`.** `{ agents, bus, focus, interactive }` + `alive()`. The
   registry becomes fleet-owned; the trunk registers itself. Wire `tui::run` /
   `headless::run` to build and read it (`tui.rs:2738`, `tui.rs:2778`).
3. **Provider onto `Agent`.** Move `ProviderHandle` off `drive`'s parameters;
   `/model` swaps the focused agent; `fork` seeds the child's own handle.
4. **Uniform spawning + cascade.** `ToolSet::peer()` gains the spawn family;
   delete the `spawns()` axis; add parent links to `AgentRegistry::Entry` and make
   `cancel`/`clear`/ceiling cascade. Drop the depth-1 language from the `agent`
   desc.
5. **`parent` collapse + dynamic focus.** Replace `is_root`/`park_when_idle` with
   `parent: Option<AgentId>` and the computed `should_park` predicate; `Esc`
   targets `fleet.focus` + subtree; `TAB`-to-talk via the `Condvar` re-check;
   focus fallback on termination.
6. **Wiki + tests, same commit.** Rewrite [[design/agents|agents]] (one axis →
   the `parent` predicate); re-stamp [[map/exarch/agent|agent]] (was `session`),
   [[map/exarch|exarch]], [[map/exarch/tools|tools]]; set this page `active`. Meta-
   tests: a grandchild's result routes to its parent; cancelling a mid-tree agent
   reaps its subtree; a per-agent `/model` swap; focus-away terminates an idle
   sub-agent; `reply` ends a focused agent and focus falls back.

## Consequences — and what this does *not* do

- **`expect_action` is dropped**, not relocated. The completion-gate nudge
  (`session.rs:66`) was the one role flag that did not fit the `parent` collapse;
  it is removed rather than threaded as a fleet flag.
- **Conversing with a sub-agent is real but ephemeral.** You may `TAB` to a *live*
  agent and talk to it; once it goes idle and you `TAB` away it reaps, and once it
  `reply`s it is gone. There is no reviving a settled agent — its tab is a readable
  transcript, not a resumable conversation.
- **`/clear` stays the focused agent's.** Clearing rebuilds the focused agent's
  context and cascades cancel to its subtree; it is not a fleet-wide reset.
- **Persistence is out of scope.** The fleet and its registry remain ephemeral,
  per [[decisions/260617_scheduled-wakeups|scheduled-wakeups]]; a durable, restart-
  surviving fleet is future work.

## Alternatives considered

- **Flat set of peers on a common bus (the original phrasing).** Rejected: a
  child's result routes to *its* parent, which is a parent edge; a flat set has no
  parent to route to, and "dies when no active agents" needs a tree to define
  whose termination matters. Uniform nodes in a tree keep the routing and drop only
  the privilege.
- **Keep `is_root` as three booleans.** Rejected: re-deriving the trunk from
  `is_root` + `park_when_idle` + a reply flag reintroduces the conflation this
  decision removes. `parent: Option<AgentId>` is the same information with no
  special case.
- **One common message bus for both events and routing.** Rejected: fan-out and
  point-to-point are different shapes; a shared channel is not a uniform drain
  policy ([[decisions/260617_scheduled-wakeups|scheduled-wakeups]]).
- **A human-less daemon fleet living on detached work.** Rejected by the no-daemon
  rule: nothing parks without a present human or a bounded schedule, so the
  registry cannot stay non-empty indefinitely with no one watching.
- **Attachment fixed at birth (focus-steering only, no conversation).** Rejected:
  the human should be able to `TAB` to any live agent and *talk*, not merely steer
  its current turn. `reply` remains the hard terminator, so dynamic attachment does
  not muddy the return contract.
- **Idle focused sub-agent lingers parked after `TAB`-away.** Rejected: it is the
  daemon by another name. A de-focused idle agent reaps at quiescence.

## See also

[[design/agents|agents]] (the roles/axes this rewrites),
[[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]
(the reply gate refined here),
[[decisions/260617_async-agent-tool|async-agent-tool]] (the async worker,
`AgentResult` delivery, the depth-1 cap superseded here),
[[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]] (the
bus that already outlives the turn),
[[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]] (the per-turn
token, generalised to per-focus + subtree),
[[decisions/260616_tool-boundary-steering|tool-boundary-steering]] (the mid-turn
vs fresh-turn drain boundary the inbox keeps),
[[map/exarch/agent|agent]], [[map/exarch/tools|tools]],
[[map/exarch|exarch]], [[design/grant|grant]] (the capability lattice a spawn's
`parent ⊓ base` ceiling runs in).
