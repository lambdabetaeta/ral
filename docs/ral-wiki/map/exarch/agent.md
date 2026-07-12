---
generated_at_commit: 668499f
generated_at_date: 2026-07-12
covers_paths: [exarch/src/agent.rs, exarch/src/agent_registry.rs, exarch/src/event.rs, exarch/src/fleet.rs, exarch/src/nudge.rs, exarch/src/digest.rs, exarch/src/config.rs]
---

# Map: exarch / agent

`agent.rs` is the turn driver. An `Agent` is the **uniform node** of a run: the
canonical [[map/exarch/frontend|event log]] (`AgentLog`), the persistent
[[map/core/shell-state|`Shell`]] behind its own `IdentityTransport` (the
canonical turn and probe vocabulary,
[[decisions/260706_enquiry-channel|enquiry-channel]]), the agent
`Capabilities`, its own inbox, tools, nudger, `cancel::Token`, an owned
hot-swappable `ProviderHandle`, the shared focused-agent handle
(`focus: Arc<AtomicU64>`, `NO_FOCUS` sentinel in `bus.rs`), and the inherited
`interactive` flag. What every node *shares* — the registry, the one
[[map/exarch/frontend|`FleetBus`]], and the transport `Engine` — lives on the
thin [[#The Fleet|`Fleet`]], not on the node
([[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]]). There is no
`Session`, no `is_root`: every distinction reduces to **position in the tree**,
read from `parent: Option<AgentId>` together with `interactive`/`focus`. Output
caps are fixed `digest.rs` constants, not per-agent state.

The **trunk** is the parent-less node (`parent = None`). An `interactive` node
built to converse — the interactive trunk, and every `/branch` tab
([[decisions/260705_branch-minimal|branch-minimal]]) — withholds `reply` and
parks for its human, but both behaviours fall out of construction and position,
never an `is_root` branch:

```
  returns(a)    ⟺  a's tool view holds reply       // fixed at construction; false only for a conversing node
  conversing(a) =  a.interactive ∧ ¬returns(a)
  park_mode(a)  =  Quiesce        if conversing(a) ∧ ¬registry.is_live(a)   // /clear or /close reaped it
                   Held           if conversing(a) ∨ focus = a.id
                   HeldByChildren if a has live descendants
                   UntilCancelled if a.schedules.armed()
                   Quiesce        otherwise
```

`returns` (`agent.rs`) is **derived from the tool view** — the single source of
truth, so the nudge layer, parking, and the advertised tools cannot disagree
([[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]).
`park_mode` (`agent.rs`, returning a `ParkMode` of `Held` / `HeldByChildren` /
`UntilCancelled` / `Quiesce`, `bus.rs`) is the `should_park` verdict: a present
human (a conversing node, or the agent the human `TAB`bed to) holds the node
parked — unless the registry no longer lists it, since an unlisted conversing
node is unreachable and parking it would be a zombie; live descendants hold it
until their results drain; a live self-schedule holds it until cancelled;
otherwise it terminates at quiescence — the one-shot contract a headless trunk
and a settled sub-agent both satisfy. `--chat` builds the trunk with no system
prompt and an empty tool view — a bare conversation, the same drive loop.

## The drive loop

Three nested loops, the same for trunk and child alike:

- `drive` — the per-agent lifetime. The trunk publishes its sticky cancel token
  for the OS-signal path (`cancel::publish`, held for the whole drive, replacing
  the old per-turn `mint_root`); a sub-agent publishes nothing, since its token
  is reached through the registry cascade, not the slot. Each pass pulls the next
  turn from this agent's [[map/exarch/frontend|inbox]] via
  `next_or_idle(|| self.park_mode(), …)`, which **re-evaluates the park verdict
  on every `Condvar` wake** — so a `TAB` that de-focuses this agent (and notifies
  its inbox) flips it from `Held` to `Quiesce` and it reaps. A genuine
  turn boundary resets the nudge latches and clears the sticky cancel token
  (`cancel::Token::reset`), so a prior turn's Esc cannot bleed into the next; a
  self-nudge is the same turn continuing and resets neither. `reply` hard-
  terminates the loop regardless of focus or a self-armed schedule — the agent
  returns its value and is gone. At the single exit the loop winds a stranded
  prompt back through `quiesce` so the next `append_user` is always admissible
  ([[invariants/turn-ends-ready|turn-ends-ready]]); the trunk then deregisters
  itself (a child is removed by its spawn site through `settle`), so the fleet
  empties when the last agent leaves. On a caught worker panic (`pump` →
  `Ok(None)`) it rebuilds the live shell's dynamic context from the
  `durable: Mobile` snapshot the worker refreshed at the last clean tool-call
  boundary, rolling the panicking call's grant/env/cwd/handler effects back while
  completed calls' bindings survive
  ([[decisions/260612_exarch-panic-recovery|panic-recovery]]; the IO half is
  core's `TurnGuard`, restored when the turn unwinds —
  [[internals/a-turn-end-to-end|a turn, end to end]]).
- `apply` — one provider round-trip loop over the agent's *own* provider
  (`self.provider.current()`, read once at the top of the turn so a `/model` swap
  lands next turn, never mid-turn): render the transcript, stream a reply through
  `provider.complete`, **admit** then append the assistant message, dispatch any
  tool calls, append their results, optionally append a drained steering prompt,
  repeat until the model emits no tool call. The admission step (`admit_assistant`,
  at the commit boundary) enforces the [[invariants/transcript-admission|transcript-admission invariant]]:
  it repairs a non-object tool-call `fn_arguments` to `{}` (X2) and substitutes a
  stub for an otherwise-empty assistant message (X7), so every committed message
  serialises to a request a strict backend accepts. `StopReason::MaxTokens`
  *with no captured tool call* raises `ProviderError::Truncated` after appending
  the partial, so a `continue` nudge keeps the work as context; *with* captured
  tool calls it dispatches them and continues instead, since returning `Truncated`
  there would strand the protocol in `AwaitingToolResults` and fail the nudge's
  next `append_user` (X6). A hard `MAX_STEPS` ceiling (250) ends a turn whose
  model never stops calling tools — the headless/autonomous counterpart to
  interactive Esc — returning `TurnOutcome::Capped`. That outcome matches no
  nudge rule, so the driver treats it as terminal; re-driving would only spend
  the ceiling again.
- `dispatch` — runs the turn's tool-call batch in order. Every tool returns a
  `SessionToolResult` synchronously; the spawn tools return a start receipt after
  launching their detached child. Once every requested tool id has a result,
  dispatch drains this agent's [[map/exarch/frontend|inbox]]'s tool-boundary
  steering. A non-slash steering prompt is appended after the complete
  tool-result batch, and the next loop asks the provider with the user's steering
  in context
  ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]]). A
  sub-agent has no human writer, so its inbox holds no steering and this is always
  empty.

Worker-reap and large-binding notices need no drain at `drive`'s top:
core's own engine pushes both as `` `notice `` surface classes at the ready
boundary of the turn that produced them
([[decisions/260706_enquiry-channel|enquiry-channel]]), decoded by
[[map/exarch/shell-eval|shell-eval]]'s `decode_surface` into `Kind::Notice` at
the emit seam. A reap notice names a worker removed by policy — the lease
chain's idle or backstop bound on a running worker, or the retention sweep
expiring a settled entry's unclaimed result — rather than one an eliminator
observed away. Transcript and TUI only — the rendered one-liner is
[[map/exarch/cards|cards]]'s `reap_card`, the completion card's sibling — never
model-facing, since delivery of a reap to the model itself is deferred
([[decisions/260705_leases-and-budgets|leases-and-budgets]]). What `drive`'s
top still runs, each pass its own ready boundary: `reconcile_service_pins`
(the protected `services` pin is (re-)born or dies here), `reap_bindings`
(below), and `check_disk_warn`.

The retention clock is the agent's **ral-call epoch** (`Agent::ral_epoch`):
incremented once at the top of every `run_shell` call — a failed eval is
still a call — and swept into the registry right after the evaluation
returns (`Shell::advance_worker_epoch` with
[[map/exarch/shell-eval|shell-eval]]'s `SETTLED_WORKER_RETENTION`), which
stamps entries first observed settled and expires the unclaimed. The counter
starts at 0, a fork's child starts its own at 0, and `/clear` does not
rewind it — the cleared registry is empty anyway, and a monotone counter is
the one the binding-lease ledger's own committed-turn clock coincides with
one-to-one (`decisions/260629_agent-binding-reaping`) — two ticks of the
same drum, read by two different ledgers. Retention notices need no
plumbing of their own: they ride the same drain above.

**The binding-lease ledger** is armed at the same two places that mint the
first durable `MobileSnapshot` — `Agent::assemble` (the trunk, every fork,
and `for_test`) and `Agent::replace_shell` (`/clear`) — each calling
`Shell::arm_binding_lease` with [[map/exarch/shell-eval|shell-eval]]'s
`BINDING_IDLE_CALLS` (256) and `LARGE_BINDING_BYTES` (1 MiB) right after
`seed_session_dir` and right before `shell.mobile_snapshot()`, so seeding,
arming, and checkpointing stay one visible sequence. The large-binding
residency nudge rides the pushed `` `notice `` channel above.
`Agent::reap_bindings`, called at the drive loop's top, keeps the prune half
host-called: `Shell::prune_idle_bindings` prunes idle top-level names and
hands back the post-prune `MobileSnapshot` (a snapshot cannot ride the
surface seam), which the agent adopts as `Agent::durable` in the same
statement it emits one compact `Notice::Prune` `Kind::Notice` naming what
fell — transcript and TUI only, the same posture as a reap notice. The
verb's signature pairs the notices with the checkpoint, so a later panic
rollback can never resurrect a name this pass just pruned.

`/resources` is the probe fold over the same accumulators
([[invariants/probe-convention|probe-convention]]): routed exactly as
`/clear` — an `InboxMsg::Command` drained at the turn boundary, handled by
the TUI's `Control` against the agent the drive loop owns —
`Agent::resource_rows` surveys what this thread may legally read — the worker
registry's running/settled split with the nearest time-to-reap and the
binding-ledger figures read as *data* through the transport's Enquiry desk
(`probe_workers` and its sibling probes,
[[decisions/260706_enquiry-channel|enquiry-channel]]), plus inbox depth per
source, the event log's mirror length and history bytes, log-dir and scratch
disk walked at invocation, and the sub-agent ceiling as a lease row — and
`emit_resources` posts one `Kind::Resources`
carrying the raw rows beside their rendered card — the `Kind::Io` pairing,
so `transcript.jsonl` records the figures. The frontend appends the rows
for the accumulators *it* owns (viewports, views, the bus) at render time;
neither half reaches across a thread. Probing never mutates and never
renews a lease — enumeration is not observation — and the fold is never
model-facing.

The inbox's per-source depth is a real quota now, not just a probe figure
([[decisions/260705_leases-and-budgets|leases-and-budgets]], "Inboxes get
quotas without silent loss"). `Mailbox`/`Inbox::push` return
`Result<(), InboxReject>` from one shared rule (`Shared::try_push`), split
by source: the three idempotent sources (`user`, `schedule`, `nudge`)
always succeed, coalescing instead of growing the queue — a
`ScheduledWakeup` replaces a still-queued wakeup for the same schedule id
(newest wins), consecutive `UserSteering` pushes merge with a blank line
(never across a slash line, which would silently change its
turn-boundary classification), and an exact-duplicate `Nudge` is a no-op.
The other four (`AgentResult`, `AgentMessage`, `Command`, `Surface`) are
quota-checked against `INBOX_SOURCE_CAP` (64) and the shared
`INBOX_TOTAL_CAP` (256) and *rejected*, never dropped, once full — every
producer surfaces the rejection to its own caller: `AgentRegistry::message`
returns `MessageError::RecipientInboxFull` (the `message` tool reports it),
a rejected slash command reports through the UI's error line, and a
rejected `spawn` completion or surfaced batch — which has no synchronous
caller left to return to — records straight to the durable
`transcript.jsonl` instead of the live bus, so holding the rejection report
never extends a bus sender's lifetime past the turn that queued it.

The headless-completion gate is gone with `expect_action`
([[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]]): the one role flag
that did not fit the `parent` collapse is dropped, not relocated. The nudges that
remain — `must_reply` for a returning agent (`returns()`), `continue` on
truncation, empty/early-stop repair, the one-shot latch that turns a headless
root's *first* `reply` back for self-verification before honouring the next,
and the one pinned-state reminder — are
driven off the same `react` rule. Live descendants make the agent wait:
`must_reply` is suspended, and pin/no-pin reminders wait too, since the agent has
already delegated the next actionable fact. Once the descendants settle, the
rules resume against the still-live pin register. The pinned-state reminder is
uniform for every pin kind (tasks, goals, protected `commitment:*` pins alike
([[decisions/260703_protected-commitment-pins|protected-commitment-pins]])) and
every actionable agent role: budget-free while anything is pinned, independent
of and additive with `must_reply` — a returning agent that finishes without
replying while it still holds a live commitment is nudged for both after its
children have landed. Exhausted transport and rate-limit failures are provider
facts, so they surface as `Kind::ProviderError` and do not post a model-visible
self-nudge.

The same agent owns the protected pin mirror read/set/clear helpers used by
`commit`/`verify_commitment`: a settled writer's formalized card, or a settled
verifier's passing result, is tagged by the worker thread that drove it, and
`Agent::settle_commitment` projects that tag — an open (`Kind::Pin`) or a
clear (`Kind::Unpin`) — on the parent's own thread as the result drains.
Ordinary model-authored `surface` cannot reach either path.

## The Fleet

`fleet.rs` is the thin run-as-a-whole: `{ agents: AgentRegistry, bus: FleetBus,
engine: Arc<provider::Engine> }`. It owns no turn logic — the trunk
and each child drive themselves; the fleet is only where the frontend reads "all
live agents" and "the bus to drain". The
frontend ([[map/exarch/frontend|`tui::run`]] / `headless::run`) builds it from
handles the trunk already minted at construction, so fleet and nodes never
disagree about what is shared.

- **The fleet is alive while the registry is non-empty** — the literal "dies
  when no active agents
  remain". An agent removes itself at termination (`reply`, quiescence, cancel);
  a conversing node stays until `/quit` (or `/close`) because it parks; a
  headless trunk leaves at quiescence; a sub-agent leaves on settle. There is no
  human-less daemon: nothing lingers without a present human, running work, or a
  bounded self-schedule.
- **Focus is dynamic.** The shared `focus: Arc<AtomicU64>` handle (minted by the
  trunk, cloned to every node and the frontend) names the attached agent;
  `NO_FOCUS` (`AgentId::MAX`, `bus.rs`) is the sentinel — off the TUI it never
  moves, and in
  the TUI it means the frontend resolves focus to the trunk. `TAB` moves it
  (the TUI), notifying the previous and new focused inboxes; the focused agent
  receives the human's typed lines as fresh turns and owns `Esc`. A de-focused
  idle agent wakes, finds `park_mode` now `Quiesce`, and reaps. A returning
  agent's `reply` cancels its proper descendants, ends it even
  mid-conversation, and the TUI then **falls focus back to its parent**,
  recursing to the trunk.

## Cancellation cascades the subtree, across both layers

The single cascade serves the deliberate teardowns — `agent_cancel`, a
returning agent's `reply`, and the `/clear` / `/close` subtree reaps.
`AgentRegistry::Entry` carries a `parent` link, so the registry is the
spawn *tree*: `AgentRegistry::cancel(id)` walks descendants and cancels the whole
subtree, `cancel_descendants(root)` abandons a returning agent's children without
advancing the global generation, and `clear_subtree(root)` reaps a subtree and
bumps the generation, so a late result or deferred surface batch from a cleared
generation is still dropped. Each cancelled node is stopped **across both
layers**: its cooperative `Token` (read by the drive loop between steps and
raced by the provider's mid-stream cancel) *and* its own session's durable root
(`eval_root: Option<DurableRoot>` on the entry, minted from
`Shell::cancel_handle` at registration), so a `ral` eval already in flight
unwinds at the evaluator's poll points instead of grinding to its
`timeout_secs` wall. The trunk registers no eval-root — its session outlives
any cancel; Esc reaches its turn through the published foreground slot
([[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]],
[[internals/cancellation|cancellation]]). `Esc` / Ctrl-C, by contrast, are a
**per-tab turn interrupt**, not a cascade: they stop only the *focused* agent's
current turn (`AgentRegistry::interrupt(id)`, or `cancel::raise_interrupt` on
the trunk), leaving its descendants running
([[decisions/260705_cancel-per-tab|cancel-per-tab]]); the focused agent's
sticky token is cleared at each turn boundary (`Token::reset`).

Cancelling `eval_root` already reaches a cancelled node's own detached `ral`
workers with no edge of its own: a worker's cancel scope is a child of its
shell's durable root, and every `CancelScope::is_cancelled` walks its
ancestors. What the cascade does *not* reach is a node that ends without
ever being cancelled — the ordinary `reply`/settle path, or the trunk's own
end-of-`drive` `deregister` — since neither touches the registry's cascade
primitive at all. `Agent`'s `Drop` closes that gap in one place: it cancels
every worker still registered on its own shell and clears its own armed
schedules unconditionally, the same law `clear` already applies explicitly
below, so a settled-but-never-cancelled agent leaks neither
([[design/residency|residency]], [[decisions/260705_session-ledger|session-ledger]]).

## The provider is per-agent and hot-swappable

`ProviderHandle` is owned by the `Agent`, not threaded through `drive`'s
parameters. `drive` reads `self.provider.current()` once per turn. `/model` swaps
the **focused** agent's handle directly on the UI thread (via the registry), so a
swap on one agent never disturbs another. `fork` seeds the child's own handle
from the parent's current provider (`ProviderHandle::new(self.provider.current())`),
so the child inherits the model in force at spawn and may diverge afterward.

## Lifecycle: clear, compact, fork

`clear` rebuilds the focused agent without carrying cancellation residue forward:
it obtains a fresh shell from `boot_root_shell` (the scratch-seeding wrapper
around `bootstrap::boot_shell`, where stale-interrupt discard and cancel
re-chaining live), truncates and restarts the event log, clears the schedule
registry, and cascades cancel to its subtree. Before the outgoing shell is
replaced, `clear` cancels every worker still registered on it
(`Shell::cancel_workers`) — explicit destruction outranks every lease, the
durable class included — reaching it through the transport while it is still
unambiguously *this* shell, since there is no way back to it once the
transport is swapped. A worker settling after the cancel still tries to flush
its deferred `done` batch through the boundary it captured before the clear;
the same generation guard (`deferred_sink`'s admission check,
[[map/exarch/shell-eval|shell-eval]]) that already drops a stale agent result
drops that flush too, so no pre-clear worker output survives into the rebuilt
context. It is the focused agent's, not a fleet-wide reset.

`compact` runs `provider.summarize` over the history when context pressure
crosses the window's reserve (`digest.rs`'s `compaction_due` — used tokens
into the top 15% of a known window; `COMPACT_THRESHOLD`, 500 KiB of serialised
history, is the fallback when the window is unknown) and `AgentLog::can_compact`
holds (no pending tool results). It is called at the **top of `apply`**, where the agent is
`ReadyForUser` ([[invariants/turn-ends-ready|turn-ends-ready]]) and the gate
actually holds — every provider round-trip passes through here, so long
autonomous and headless turns stay bounded without an interactive `/compact`. A
turn-boundary Esc bails before the summarize request
([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]).
A successful `AgentLog::apply_compaction` physically drains
`events[..suffix_start]` from the in-memory mirror after the archival
`Compacted` breadcrumb is durably recorded — `event_count`/`history_bytes`
shrink to summary + suffix, heap reclamation rather than just a narrower
read-time view. `events.json` is append-only and untouched either way: a
failed compaction (tool results pending) drops nothing, and a successful
one never rewrites what is already on disk
([[decisions/260705_leases-and-budgets|leases-and-budgets]], "Compaction
physically drops the model prefix in memory").

`Agent::check_disk_warn` is the disk half of the same ADR ("Disk: report
and warn only") — report-and-warn only, never rotation or deletion.
Unconfigured (`config::disk_warn_bytes` absent, the default) it is a no-op
by construction: no walk, no cost, ever. Configured, it rides the same
`ral_epoch` the settled-worker and binding-lease sweeps already read,
amortized to once every `DISK_WARN_CHECK_INTERVAL` (32) calls, at the same
ready boundary as `reconcile_service_pins`/`reap_bindings` in `drive`'s loop.
Crossing the ceiling (session log dir + `EXARCH_SCRATCH`, summed via the
existing `resources::dir_size`) emits one `Kind::SystemNote`, latched until
a later check finds the total back under — one warning per excursion, not
one per boundary.

`fork` builds the child `Agent` for [[design/agents|sub-agent spawning]] through
`Shell::fork_session` ([[map/core/shell-state|the flow matrix]]) rather than
hand-copying fields after a bare `Shell::new`. It takes the child's
`Capabilities` **as an argument**, so the spawn site owns the authority decision
(the parent's verbatim, or `parent ⊓ base` via [[map/exarch/policy|`policy::narrow`]]).
The child sets `parent: Some(self.id)` — the tree edge that routes its result and
drives the subtree cascade — and registers itself in the fleet's shared registry.
It snapshots the parent's whole lexical scope (prelude, agent library, every
accumulated binding), its dynamic context (cwd, env, grants, handlers), and the
installed builtin table, and starts fresh in everything else — fresh control
counters and a freshly-defaulted `SessionState`, so it holds **no terminal
authority** (`TerminalAccess::Denied`, no lease — a sub-agent is not the
foreground agent and can never seize the controlling terminal the TUI owns).
There is no flow-back: the child's `cd`, env, and new bindings die with it. Every
agent may spawn, but `fork` also computes the child's `fuel` as
`self.fuel.saturating_sub(1)` (`SPAWN_FUEL = 3` at the trunk); a `fuel == 0`
agent's `tools_for` view drops `amnemon`/`mnemon`
([[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]]), so a delegation
chain bottoms out a fixed number of generations down. The fork mirrors on the
bus as `Kind::Born` / `Kind::Died` regardless of remaining fuel.

`fork` is `fork_with(caps, returns: true)` — the *returning*-child
constructor; `branch` is `fork_with(self.caps, returns: false)` plus
`inherit_context`, minting a *conversing* peer tab with the parent's verbatim
authority ([[decisions/260705_branch-minimal|branch-minimal]]).
`fork_remembering` is the mnemon variant: the same fork plus
`inherit_context`, which asks `AgentLog` to import the parent's model-visible
context ([[decisions/260702_subagent-memory-modes|subagent-memory-modes]]). The
spawn site still seeds the tool call's prompt through the child's inbox, so the
prompt enters through the same turn path as amnemon. `AgentLog` drops a pending
unanswered assistant tool-call frame when the parent is mid-dispatch, so the
child inherits a request context rather than a dangling provider protocol. The
amnemon path uses plain `fork`; both spawn modes seed the launch prompt into
the child's inbox.

Routing the fork through core matters because the builtin table is the easiest
thing to drop. The exarch host builtins — `view-text`, `grep-files`,
`edit-hash`, `edit-replace`, `explore-dir`, `fff`, the skill loaders
([[map/exarch/builtins|builtins]]) — live in the agent's dispatch table,
*outside* `Mobile`, and the `view-text-around` helper in `agent.ral` calls
`view-text`. A fork that copied only `mobile.scope` and `mobile.context` would
leave the child's `view-text-around` resolving to nothing and falling through
to a failed PATH lookup. `fork_session` copies `agent.builtins` as part of the
flow matrix, so the decision lives in one place and the table cannot be
silently severed at this call site.

`digest.rs` holds `clip` and the fixed per-section byte caps for what the
*model* sees in history: each tool-result section has its own cap
(`VALUE_CAP` 20 KiB, `STDOUT_CAP`/`STDERR_CAP` 10 KiB), alongside separate caps
for opaque error blobs (`OPAQUE_CAP`), agent replies (`AGENT_REPLY_CAP`), and
the history-compaction threshold. An oversize section keeps a head+tail digest
and elides the middle, with a banner nudging the model to scope the query at
its source and re-read in slices — the same rendering the transcript records,
so the user never sees more of a result than the model does. `run_shell` here
threads to [[map/exarch/shell-eval|shell-eval]].

## See also

[[design/agents|agents]] (the role model these nodes realise — one `parent`
predicate, the conversing trunk vs returning agents),
[[decisions/260624_uniform-agent-nodes|uniform-agent-nodes]] (the Fleet/Agent
split this page maps), [[map/exarch/tools|tools]] (the registry and gates the
provider sees), [[map/exarch/frontend|frontend]] (the bus, the inbox, the
registry, and the two frontends), [[map/exarch/provider|provider]],
[[map/exarch/policy|policy]], [[map/exarch|exarch]],
[[design/residency|residency]] (the resident ledger this cascade and the
worker/schedule teardown edge are chapters of).
