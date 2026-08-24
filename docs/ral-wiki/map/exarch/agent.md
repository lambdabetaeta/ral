---
generated_at_commit: cbeb5457
generated_at_date: 2026-08-17
covers_paths: [exarch/src/agent.rs, exarch/src/agent/, exarch/src/fleet.rs, exarch/src/fleet/desk.rs, exarch/src/fleet/registry.rs, exarch/src/prompt.rs, exarch/src/config.rs, exarch/src/net_policy.rs, exarch/src/net_policy/, exarch/src/egress.rs]
---

# Map: exarch / agent

`agent.rs` defines the node every fleet member shares. An `Agent` is the
**uniform node** of the fleet: the canonical
[[internals/session-record|model projection]] (`AgentLog`), a **seat**
(`agent/seat.rs`) carrying the transport each run drives through —
`Seat::Identity`, the persistent in-process [[map/core/shell-state|`Shell`]]
behind an `IdentityTransport` (plus the session `Scratch`, the re-seed cwd,
and the interrupt target the registry interrupts through), or `Seat::Wire`, a
`WireTransport` driving a remote engine, one process per session
([[decisions/260722_session-is-a-process|session-is-a-process]]) — the
canonical run and probe vocabulary either way
([[decisions/260706_enquiry-channel|enquiry-channel]]), the agent
`Capabilities`, its own inbox, nudger, `cancel::Token`, an owned
hot-swappable `ProviderHandle`, and the inherited
`interactive` flag. What every node *shares* — the registry, the one
[[map/exarch/frontend|`FleetBus`]], and the transport `Engine` — lives on the
thin [[#The Fleet|`Fleet`]], not on the node. There is no
`Session`, no `is_root`: every distinction reduces to **position in the tree**,
read from `parent: Option<AgentId>` together with `interactive` and the
registry's own engagement state. Output
caps are fixed `agent/digest.rs` constants, not per-agent state.

The **trunk** is the parent-less node (`parent = None`), built by
`Agent::root(RootConfig, RootSeat, provider)`: `RootConfig` carries the
prompt, caps, `fuel` (exarch's and synod's launch sites pass `SPAWN_FUEL`),
and the IT-set `Egress` (`exarch/src/egress.rs`) — opened once at
launch and inherited verbatim by every fork — while `RootSeat` picks the seat
kind
(`Identity` boots its own shell from `scratch`; `Wire` adopts a built
transport whose engine lives elsewhere, and refuses sub-agent forks at the
desk).

`Egress` bundles the two things a fleet's outbound network shares across
every fork: `net_policy::NetPolicy` (`exarch/src/net_policy.rs`, the IT-owned
allowlist read from `/etc/exarch/net-policy.ral` or the embedded default —
an exact `hosts` list of lowercase ASCII DNS names plus `search`, with the
retired `read`/`write`, `max-bytes` and `rate-per-minute` keys now hard
errors naming their replacement) and an `AuditLog` (`exarch/src/egress.rs`),
reduced to one `Tunnel` record per attempt: its final vetted address, and on
close the byte count each direction carried — telemetry, not policy. None of
it is a model-facing verb any more — there is no `fetch-url` builtin
([[map/exarch/builtins|builtins]]). The same `Egress` a trunk opens at launch
is what `guest-net::Config::egress` takes: the policy and ledger a synod
session's guest network is gated by are the fleet's own, not a second
copy ([[design/egress|egress]], [[map/synod|synod]]). Host-mode exarch, which
has no guest to police, still keeps `Egress` for one thing: the `search` bit
that clamps the harness `` agents `start [... search: …] `` field
([[map/exarch/builtins|builtins]]).

An `interactive` node
built to converse — the interactive trunk, and every `/branch` tab
([[decisions/260705_branch-minimal|branch-minimal]]) — withholds `reply` and
parks for its human, but both behaviours fall out of construction and position,
never an `is_root` branch:

```
  returns(a)    // fixed at construction: fork true, branch false, trunk ¬interactive
  conversing(a) =  a.interactive ∧ ¬returns(a)
  park_mode(a)  =  Quiesce        if conversing(a) ∧ ¬registry.is_live(a)   // /close reaped it
                   Held           if conversing(a)                          // immune to cancellation
                   Engaged        if a.parent ∧ returns(a) ∧ registry.engaged(a)  // parks, but a terminate cause still ends it
                   HeldByChildren if a has live descendants
                   UntilCancelled if a.schedules.armed()
                   Quiesce        otherwise
```

`returns` (`agent.rs`) is a **construction-fixed field** — one bit read by
`returns()`, parking's conversing predicate, the desk's `reply` refusal
([[map/exarch/builtins|builtins]]), and the prompt's per-agent resolver. The
resolver applies the index from `returns`, `allow_schedule`, and
`spawns = fuel > 0`, so no agent is shown a verb or section the desk would
certainly refuse; the nudge layer, parking, reply availability, and advertised
vocabulary cannot disagree
([[decisions/260623_reply-terminates-returning-agents|reply-terminates-returning-agents]]).
`park_mode` (`agent/attend.rs`, returning a `ParkMode` of `Held` / `Engaged` /
`HeldByChildren` / `UntilCancelled` / `Quiesce`, `bus/inbox.rs`) is the `should_park`
verdict: a conversing node parks `Held`, immune to cancellation; a returning,
parented agent the registry has recorded a human exchange with parks
`Engaged`, the same wait except a terminate-class cause still ends it, since
an exchange is not a conversation — unless the registry no longer lists it,
since an unlisted conversing node is unreachable and parking it would be a
zombie; live descendants hold it
until their results drain; a live self-schedule holds it until cancelled;
otherwise it terminates at quiescence — the one-shot contract a headless trunk
and a settled sub-agent both satisfy. `--chat` builds the trunk with no system
prompt, no tool at all (`tool_enabled: false`), and no nudge registry — a bare
conversation, the same attend loop.

## The attend loop

Three nested loops, the same for trunk and child alike:

- `attend` — the per-agent lifetime. The trunk publishes its sticky cancel token
  for the OS-signal path (`cancel::publish`, held for the whole attend); a
  sub-agent publishes nothing, since its token
  is reached through the registry cascade, not the slot. Each pass pulls the next
  item from this agent's [[map/exarch/frontend|inbox]] via
  `next_or_idle(|| self.park_mode(), …)`, which **re-evaluates the park verdict
  on every `Condvar` wake** — so the idle lease's terminate-cause cancel, or the
  last live child settling, is seen on the very next wake — and hands it to
  `take_up`, the per-item step shared with `attend`'s bounded
  twin `attend_backlog` (converse's per-exchange drain): generation admission, the
  exchange-boundary latch reset, a session command's dispatch to `Control`, the
  `deliberate` call itself, and the nudge reaction. A genuine
  exchange boundary resets the nudge latches and clears the sticky cancel token
  (`cancel::Token::reset`), so a prior exchange's Esc cannot bleed into the next; a
  self-nudge is the same exchange continuing and resets neither. `reply` hard-
  terminates the loop regardless of focus or a self-armed schedule — the agent
  returns its value and is gone. At the single exit the loop winds a stranded
  prompt back through `quiesce` so the next `append_user` is always admissible
  ([[invariants/turn-ends-ready|exchange-ends-ready]]); the trunk then deregisters
  itself (a child is removed by its spawn site through `settle`), so the fleet
  empties when the last agent leaves. A panic `take_up` catches around its
  `deliberate` call is a
  *host-side* fault (provider transport, surface decode, render, digest),
  recorded as `AgentOutcome::Failed`; an eval-side panic never unwinds this far —
  the engine's own run door (`Shell::run`) checkpoints the `Mobile`
  at entry, rolls it back, and reports the failed run, durability being
  engine-owned ([[decisions/260612_exarch-panic-recovery|panic-recovery]]).
  The per-call desk install retires on *every* exit, panic included, via
  `seat::RunGuard`.
- `deliberate` — one prompt stepped to quiescence over the agent's *own* provider
  (`self.provider.current()`, read once at the top by `take_up` so a `/model` swap
  lands next item, never mid-deliberation): render the transcript, stream a reply through
  `provider.complete` (one `step`), **admit** then append the assistant message, run the
  resulting tool-call batch (`run_batch`), append their results, optionally append a drained steering prompt,
  repeat until the model emits no tool call. The admission step (`admit_assistant`,
  at the commit boundary) enforces the [[invariants/transcript-admission|transcript-admission invariant]]:
  it repairs a non-object tool-call `fn_arguments` to `{}` (X2) and substitutes a
  stub for an otherwise-empty assistant message (X7), so every committed message
  serialises to a request a strict backend accepts. `StopReason::MaxTokens`
  *with no captured tool call* raises `ProviderError::Truncated` after appending
  the partial, so a `continue` nudge keeps the work as context; *with* captured
  tool calls it dispatches them and continues instead, since returning `Truncated`
  there would strand the protocol in `AwaitingToolResults` and fail the nudge's
  next `append_user` (X6). A hard `MAX_STEPS` ceiling (250) ends a deliberation whose
  model never stops calling tools — the headless/autonomous counterpart to
  interactive Esc — returning `deliberate::Outcome::Capped`. That outcome matches no
  nudge rule, so `attend` treats it as terminal; re-attending would only spend
  the ceiling again.
- `run_batch` — runs one step's tool-call batch in order, each call through
  `invoke`. Every call returns a
  `SessionToolResult` synchronously — and there is only `ral` to call
  ([[map/exarch/tools|tools]]); a spawn verb inside it hands the script a start
  receipt after launching the detached child. Once every requested tool id has
  a result,
  `run_batch` drains this agent's [[map/exarch/frontend|inbox]]'s tool-boundary
  steering. A non-slash steering prompt is appended after the complete
  tool-result batch, and the next step asks the provider with the user's steering
  in context
  ([[decisions/260616_tool-boundary-steering|tool-boundary-steering]]). A
  sub-agent has no human writer, so its inbox holds no steering and this is always
  empty.

  A streamed step closes through one `close_step` door: it seals the answer and
  reasoning choppers, then publishes `Transient::Boundary`, even when the
  provider or cancellation path already carries an error. `abandon_step` uses
  the same ordering best-effort, so a live display edge cannot survive into the
  next step ([[map/exarch/frontend|frontend]]).

Worker-reap and large-binding notices need no drain at `attend`'s top:
core's own engine pushes both as `` `notice `` surface classes at the ready
boundary of the run that produced them
([[decisions/260706_enquiry-channel|enquiry-channel]]), decoded by
[[map/exarch/shell-eval|shell-eval]]'s `decode_surface` into `Surface::Notice`
and recorded/rendered from there. A reap notice names a worker removed by policy — the lease
chain's idle or backstop bound on a running worker, or the retention sweep
expiring a settled entry's unclaimed result — rather than one an eliminator
observed away. Transcript and TUI only — the rendered one-liner is
[[map/exarch/cards|cards]]'s `reap_card`, the completion card's sibling — never
model-facing, since delivery of a reap to the model itself is deferred.
What `attend`'s
top still runs, each pass its own ready boundary: `reconcile_service_pins`
(the protected `services` pin is (re-)born or dies here) and
`check_disk_warn`.

The retention clock itself is core's: the engine ticks the worker registry
once per source dispatch and sweeps it at each settled run's ready
boundary ([[map/core/shell-state|shell-state]]), armed with
[[map/exarch/shell-eval|shell-eval]]'s `SETTLED_WORKER_RETENTION`. The
agent keeps its own mirror of the same drum — `Agent::ral_epoch`,
incremented once at the top of every `run_shell` call, a failed eval still
a call — which `/resources` reads to render nearest time-to-reap and
`check_disk_warn` reads for its amortisation; the two clocks coincide
one-to-one (`decisions/260629_agent-binding-reaping`). The counter starts
at 0, a fork's child starts its own at 0, and `/clear` does not rewind it.
Retention notices need no plumbing of their own: they ride the same drain
above.

**The binding-lease ledger** is armed by `bootstrap::arm_session_ledgers` —
the one policy site, run by the identity seat's ceremony right after the
session-dir seeding (seeding then arming stay one visible sequence) and by
the wire engine's own boot (`engine_boot_shell`) — with
[[map/exarch/shell-eval|shell-eval]]'s `BINDING_IDLE_CALLS` (256) and
`LARGE_BINDING_BYTES` (1 MiB), beside `arm_worker_retention`. The
large-binding residency nudge rides the pushed `` `notice `` channel above,
and the prune half is engine housekeeping too: idle top-level names fall at
the engine's own ready boundary, announced as a pushed
`` `notice [kind: `prune] `` class the host decodes into the same
recorded `Display::Notice` posture as a reap. The engine's
run-entry checkpoint orders after any prior boundary's prune, so a later
panic rollback can never resurrect a name a pass just pruned.

`/resources` is the probe fold over the same accumulators
([[invariants/probe-convention|probe-convention]]): routed exactly as
`/clear` — an `Item::Command` drained at the exchange boundary, handled by
the TUI's `Control` against the agent the attend loop owns —
`Agent::resource_rows` surveys what this thread may legally read — the worker
registry's running/settled split with the nearest time-to-reap and the
binding-ledger figures read as *data* through the transport's Enquiry desk
(`probe_workers` and its sibling probes,
[[decisions/260706_enquiry-channel|enquiry-channel]]), plus inbox depth per
source, the event ledger's logical length and history bytes, log-dir and scratch
disk walked at invocation, and the sub-agent idle lease as two rows (nearest
time-to-reap, and the demote threshold) — and
`emit_resources` posts one `Transient::Resources`
carrying the raw rows beside their already-rendered card together — chrome
only, so unlike a recorded observation there is no later fold to re-render one
from.
A probe fold is an interactive diagnostic, read when it is run: no session
keeps a pressure history, so the figures live only in this emission, never
in `record.jsonl`. The frontend appends the rows for the accumulators *it*
owns (viewports, views, the bus) at render time; neither half reaches
across a thread. Probing never mutates and never renews a lease —
enumeration is not observation — and the fold is never model-facing.

The inbox's per-source depth is a real quota now, not just a probe figure.
`Mailbox`/`Inbox::push` return
`Result<(), InboxReject>` from one shared rule (`Shared::try_push`), split
by source: the three idempotent sources (`user`, `schedule`, `nudge`)
always succeed, coalescing instead of growing the queue — a
`ScheduledWakeup` replaces a still-queued wakeup for the same schedule id
(newest wins), consecutive `UserSteering` pushes merge with a blank line
(never across a slash line, which would silently change its
exchange-boundary classification), and an exact-duplicate `Nudge` is a no-op.
The other four (`AgentResult`, `AgentMessage`, `Command`, `Surface`) are
quota-checked against `INBOX_SOURCE_CAP` (64) and the shared
`INBOX_TOTAL_CAP` (256) and *rejected*, never dropped, once full — every
producer surfaces the rejection to its own caller: `AgentRegistry::message`
returns `MessageError::RecipientInboxFull` (`` agents `message `` reports it),
a rejected slash command reports through the UI's error line, and a
rejected `spawn` completion or surfaced batch — which has no synchronous
caller left to return to — records straight through the record seam as a
`Transient::Fault` instead of the live bus, so holding the rejection report
never extends a bus sender's lifetime past the run that queued it.

The headless-completion gate is gone with `expect_action`: the one role flag
that did not fit the `parent` collapse is dropped, not relocated. The nudges that
remain — `must_reply` for a returning agent (`returns()`), `continue` on
truncation, empty/early-stop repair, the one-shot latch that turns a headless
root's *first* `reply` back for self-verification before honouring the next,
the one pinned-state reminder, and the context-pressure warning
([[decisions/260805_context-pressure-is-a-nudge|context-pressure-is-a-nudge]]:
budget-free, latched once per crossing of the soft line one reserve ahead of
auto-compaction, re-armed when the measured context crosses back below the line
or the session resets) — are
driven off the same `react` rule. Live descendants make the agent wait:
`must_reply` is suspended, and pin/no-pin reminders wait too, since the agent has
already delegated the next actionable fact — though the pressure warning does
not wait, the swelling context being the agent's own. Once the descendants settle, the
rules resume against the still-live pin register. The pinned-state reminder is
uniform for every pin kind (tasks, goals, any other pinned state alike) and
every actionable agent role: budget-free while anything is pinned, independent
of and additive with `must_reply` — a returning agent that finishes without
replying while it still holds pinned state is nudged for both after its
children have landed. Reporting an attempt is not the nudger's job at all:
`take_up` emits `Forensic::ProviderError` for whatever error the attempt carries,
before it asks for a nudge and whether or not one follows, so `react` decides
and nothing more.

A `--chat` trunk holds **no nudge registry** (`nudges: Option<Registry>`, `None`
when the tool is withheld): every nudge steers the model toward a tool it does
not have, so no rule runs, no reminder fires, and nothing synthetic ever joins
the conversation. Its provider errors still reach the human, since that report
is the attend loop's own step.

## The Fleet

`fleet.rs` is the thin run-as-a-whole: `{ agents: AgentRegistry, bus: FleetBus,
engine: Arc<provider::Engine> }`. It owns no execution logic — the trunk
and each child attend to themselves; the fleet is only where the frontend reads "all
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
- **The idle lease is dynamic; focus is not.** A leased child
  (`Registration::lease`, armed only for a returning sub-agent) is reaped once
  its idle span — measured off the registry's last-human-exchange clock,
  seeded at birth — exceeds its bound: the reaper (`fleet/registry.rs`'s
  `lease_fire`) re-arms itself for the remaining margin on every fire that
  finds the entry still live and under bound, and cascades the subtree with
  `CancelCause::Deadline` once it is not. The one thing that renews the clock
  is a delivered human message (`AgentRegistry::steer`); nothing else does —
  not the TUI's `TAB` cursor, a plain, presentation-only `AgentId` local to
  the frontend (`tui::tabs::Tabs::focus`) that neither the registry nor
  `park_mode` ever reads, not the model-facing `` agents `message `` tag, not a
  `/resources` probe. A returning agent's `reply` cancels its proper
  descendants and ends it even mid-conversation, regardless of which tab the
  human's cursor sits on.

## Cancellation cascades the subtree, across both layers

The single cascade serves the deliberate teardowns — `` agents `cancel ``, a
returning agent's `reply`, and the `/clear` / `/close` subtree reaps.
`AgentRegistry` lives in `fleet/registry.rs`; an entry's `name` is its
identity — unique among live entries, enforced at `register`
(`RegisterError::NameTaken`; the trunk holds `TRUNK_NAME`) and the handle
`` agents `message ``/`` agents `cancel `` resolve descendants by. Each entry
carries a `parent` link, so the registry is the
spawn *tree*: `AgentRegistry::cancel(id)` walks descendants and cancels the whole
subtree, `cancel_descendants(root)` abandons a returning agent's children without
touching any generation, and `clear_subtree(root)` reaps a subtree and bumps
`root`'s *own* generation, so a late result or deferred surface batch addressed
to that root is still dropped. The counter is per entry, not per fleet, and the
number a worker carries is its *reader's* — a `/clear` in one tab must not throw
away work another tab is still waiting on. Each cancelled node is stopped **across both
layers**: its cooperative `Token` (read by `deliberate` between steps and
raced by the provider's mid-stream cancel) *and* the eval layer through the
entry's `reach: EvalReach` (`fleet/registry.rs`, minted from the
seat at registration — every entry carries one, the trunk included) —
`EvalReach::Identity` holds the session's durable
root (`Shell::cancel_handle`) for `terminate` and, for `interrupt`, the cell its
transport publishes each dispatch's scope into as that dispatch is minted —
ahead of the engine lock, so an interrupt racing a dispatch still waiting on the
lock reaches the run about to be born rather than the one just ended
([[internals/cancellation|cancellation]]) — while `EvalReach::Wire`'s only
host-reachable primitive is
`Control::Cancel` on the in-flight dispatch, so both motions resolve to it
— and a `ral` eval already in flight
unwinds at the evaluator's poll points instead of grinding to its
`timeout_secs` wall. The trunk's reach is *interrupt-only* —
`EvalReach::interrupt_only` clears its `eval_root` to `None` at registration,
so a `terminate` there degrades to the `Token` alone: its session outlives any
cancel, and a captured root would both permanently poison it and go stale at
the next `/clear`, which rebuilds the trunk's shell in place while an entry is
registered once, at birth. Esc also reaches the trunk's exchange through the
ambient foreground cause, which only the trunk's session is minted facing
([[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]],
[[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]],
[[internals/cancellation|cancellation]]). `Esc` / Ctrl-C, by contrast, are a
**per-tab exchange interrupt**, not a cascade: they stop only the *focused* agent's
current exchange (`AgentRegistry::interrupt(id)`, plus `cancel::raise_interrupt`
on the trunk), leaving its descendants running
([[decisions/260705_cancel-per-tab|cancel-per-tab]]); the focused agent's
sticky token is cleared at each exchange boundary (`Token::reset`).

Cancelling `eval_root` already reaches a cancelled node's own detached `ral`
workers with no edge of its own: a worker's cancel scope is a child of its
shell's durable root, and every `CancelScope::is_cancelled` walks its
ancestors. What the cascade does *not* reach is a node that ends without
ever being cancelled — the ordinary `reply`/settle path, or the trunk's own
end-of-`attend` `deregister` — since neither touches the registry's cascade
primitive at all. `Agent`'s `Drop` closes that gap in one place: it cancels
every worker still registered on its own shell and clears its own armed
schedules unconditionally, the same law `clear` already applies explicitly
below, so a settled-but-never-cancelled agent leaks neither
([[design/residency|residency]], [[decisions/260705_session-ledger|session-ledger]]).

## The provider is per-agent and hot-swappable

`ProviderHandle` is owned by the `Agent`, not threaded through `attend`'s
parameters. `take_up` reads `self.provider.current()` once per item. `/model` swaps
the **focused** agent's handle directly on the UI thread (via the registry), so a
swap on one agent never disturbs another. `fork` seeds the child's own handle
from the parent's current provider (`ProviderHandle::new(self.provider.current())`),
so the child inherits the model in force at spawn and may diverge afterward.

## Lifecycle: clear, compact, resume, fork

`clear` rebuilds the focused agent without carrying cancellation residue forward:
it drops the waiting inbox before the reboot, so a prompt typed during that
reboot belongs to the new context, then re-runs the seat's ceremony
(`Seat::clear`) — the identity seat reboots a
fresh shell from `boot_root_shell` (`agent/seat.rs`, the cwd- and
scratch-seeding wrapper over `bootstrap::boot_shell`) onto the *same*
interrupt target; a wire session instead clears by killing its engine
process and booting a fresh one from the same recipe, so no caller routes
`/clear` to that seat. The identity seat rotates `record.jsonl` to the
first-free `.n`, then starts a fresh record ledger; it never truncates the
old record. The record rename is the rotation commit point. The rotation
swaps the *file* behind the seam, never the seam: the `Emitter` and its
attached bus sink are the session's, so every clone already handed out — the
surface buffer's, the frontend's coupled channel — writes into the new
segment and keeps publishing. Swapping the `Emitter` instead stranded them
on the rotated-away file, and the frontend, whose clear-drain gate opens on
the `Transient::Cleared` that seam publishes, stayed dark for the whole first
exchange of the cleared session. Clear also
clears the schedule registry and cascades cancel to its subtree. Replacing the
transport drops the outgoing shell, whose `LocalState` teardown cancels
every worker still registered on it — explicit destruction outranks every
lease, the durable class included, with no host call site to forget
([[map/core/shell-state|shell-state]]). A worker settling after the cancel still tries to flush
its deferred `done` batch through the boundary it captured before the clear;
the same generation guard (`deferred_sink`'s admission check,
[[map/exarch/shell-eval|shell-eval]]) that already drops a stale agent result
drops that flush too, so no pre-clear worker output survives into the rebuilt
context. It is the focused agent's, not a fleet-wide reset.

`--resume` is a trunk-only lifecycle: it validates and replays session 0's
`record.jsonl`, quarantining only an unterminated crash fragment, then reopens
the file for append. The live model and provider are selected again, while the
shell is fresh and receives a note describing the bindings, workers, cwd,
scratch, pins, and schedules that were not durable. Wire seats and child logs
are refused rather than half-resumed.

`--no-logs` chooses the mirror-only path at birth: no durable event ledger or
transcript, and no run lock, is created. The in-memory model view and live bus
still operate, children inherit the choice, and there is consequently nothing
for `--resume` to reopen.

`compact` runs `provider.summarize` over the closed prefix when context pressure
crosses the window's reserve (`digest.rs`'s `compaction_due` — used tokens
into the top 15% of a known window; `COMPACT_THRESHOLD`, 500 KiB of serialised
history, is the fallback when the window is unknown) and `AgentLog::can_compact`
holds (no pending tool results). It is called at the **top of `deliberate`**,
where the agent is `ReadyForUser` ([[invariants/turn-ends-ready|exchange-ends-ready]])
and the gate actually holds — every provider round-trip (`step`) passes through
here, so long autonomous and headless sessions stay bounded without an
interactive `/compact`. An exchange-boundary Esc bails before the summarize
request ([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]).

A successful compaction records `ContextEdited { op: Fold, by: Harness }` with
the digest and cut. The same fold step removes the prefix spans from the model
view and evicts their resident events, retaining only their byte ranges in
`record.jsonl`; this is residency following the view, not a second drain rule.
There is no `Compacted` state or archival cut to infer. A failed attempt records
nothing and sheds nothing. The durable log is appended to, never rewritten.

Auto-compaction is one authority over the same `Fold`/`Drop` edit
([[decisions/260812_context-is-a-projection|context-is-a-projection]]). The
model reaches the other two: `context-read` and `context-drop` let it survey
and shed closed exchanges of its own choosing, each recording `ContextEdited`
with `EditAuthority::Model` rather than `Harness`. The user's own hand is
`/rewind <exchange>`, which desugars to the same `Drop` at
`EditAuthority::User`, sheds queued self-nudges, and resets the nudge budget;
`/context` surveys the transcript without editing it, the read-only sibling
`ReplControl::command` serves alongside `/clear`, `/compact`, `/branch`, and
`/quit`.

`Agent::check_disk_warn` is the disk half of the same ADR ("Disk: report
and warn only") — report-and-warn only, never rotation or deletion.
Unconfigured (`config::disk_warn_bytes` absent, the default) it is a no-op
by construction: no walk, no cost, ever. Configured, it rides the same
`ral_epoch` the settled-worker and binding-lease sweeps already read,
amortized to once every `DISK_WARN_CHECK_INTERVAL` (32) calls, at the same
ready boundary as `reconcile_service_pins` in `attend`'s loop.
Crossing the ceiling (session log dir + `EXARCH_SCRATCH`, summed via the
existing `resources::dir_size`) emits one `Forensic::SystemNote`, latched until
a later check finds the total back under — one warning per excursion, not
one per boundary.

A fork builds the child `Agent` for [[design/agents|sub-agent spawning]] through
`Shell::fork_session` ([[map/core/shell-state|the flow matrix]]) rather than
hand-copying fields after a bare `Shell::new`. It takes the child's
`Capabilities` **as an argument**, so the spawn site owns the authority decision
(the parent's verbatim, or `parent ⊓ base` via [[map/exarch/policy|`policy::narrow`]]).
The child sets `parent: Some(self.id)` — the tree edge that routes its result and
drives the subtree cascade — and registers itself in the fleet's shared registry.
It snapshots the **serialisable fragment** of the parent's lexical scope
(prelude, agent library, every accumulated binding that has a wire form —
`fork_into_nursery` scrubs `Value::Handle` bindings before parking the fork,
so an identity fork and a wire hatch's `EngineSeed` agree,
[[design/agents|agents]]), its dynamic context (cwd, env, grants, handlers),
and the installed builtin table, and starts fresh in everything else — fresh
control counters and a freshly-defaulted `SessionState`, so it holds **no
terminal authority** (`TerminalAccess::Denied`, no lease — a sub-agent is not the
foreground agent and can never seize the controlling terminal the TUI owns).
There is no flow-back: the child's `cd`, env, and new bindings die with it. An
agent with fuel left may spawn, and each fork hands the child one less unit of
`fuel` than the parent holds (`SPAWN_FUEL = 3` at the trunk; the parent's own
fuel is never debited, so fuel bounds depth, not fan-out). At `fuel == 0` the
prompt drops the spawn family — `agents` — and the desk refuses `agent-start`
with the exhaustion text; the desk remains the runtime wall
([[decisions/260703_spawn-fuel-ceiling|spawn-fuel-ceiling]]) — so a delegation
chain bottoms out by refusal a fixed number of generations down. The fork
mirrors on the bus as `Transient::Born` / `Transient::Died` regardless of
remaining fuel.

`fork_with(caps, returns)` is the shared fork core — a returning child passes
`true`; `branch` is `fork_with(self.caps, returns: false)` plus
`inherit_context`, minting a *conversing* peer tab with the parent's verbatim
authority ([[decisions/260705_branch-minimal|branch-minimal]]). A builtin
spawn takes the decomposed path instead: the `` `start `` tag's body forks the
session into the run's nursery (`Shell::fork_into_nursery`), and the desk's
`agent-start` arm adopts it and calls `Agent::assemble` at one less unit of
fuel ([[map/exarch/builtins|builtins]]).

Prompt resolution is shared across the root, identity-fork, and wire-child
paths. Each keeps the unresolved base and applies its own `returns`,
`allow_schedule`, and child-fuel bits; the resolver appends `Agents`
iff fuel remains and `Agent` iff the child returns. The child's log bookend
records that fully resolved prompt length, including the filtered index and
late sections, so a child never inherits an already-appended `Agent` section.

### Wire-seat spawn: the desk's two-phase arm

`Seat::Identity`'s `agent-start` adopts a nursery-parked fork directly, in
the same process. A `Seat::Wire` trunk's desk cannot: the nursery lives in
the guest engine, and the desk runs host-side. So the enquiry becomes
**two-phase**, both phases the same `` `agent-start `` vocabulary an identity
trunk speaks — the arm is chosen on a stated fact (the seat's kind, read at
`assemble`), never inferred:

1. `` `agent-start [session, kind, prompt, name, grant, search] `` — every
   authority check the desk runs today (generation, fuel, name collision,
   grant-tag validity) runs first, before any process exists. On a wire
   trunk it mints a token, registers a pending hatch reserving the name, and
   answers `` `hatch [token, port] ``.
2. The builtin runs the guest-side **hatch** (core's Unix-only
   `core/src/hatch.rs`) —
   dial the host on the named port, write the 16-byte preamble
   (`vm-manager/src/preamble.rs`: 8-byte magic `b"ralagent"` + the `u64`
   token, little-endian), spawn `current_exe --engine` with the dial on its
   protocol fd and an `EngineSeed` on an inherited one — then enquires
   `` `agent-hatched [token] ``. The desk's handler awaits the correlated
   dial through its **hatchery** (`vm_manager`-free by construction — a
   capability object `RootConfig` carries, `None` for identity trunks),
   reads and checks the preamble through core's portable
   `core/src/hatch_preamble.rs`, adopts the stream as `Seat::Wire`, and
   hands the child to the same `spawn_async` an identity fork reaches, at
   `fuel = parent - 1`. A wire trunk with fuel > 0 and no hatchery is a
   construction error, refused at `Agent::root` with a sentence.

A refused phase 1 spawns nothing; a phase 2 that times out kills and reaps
the hatched child before reporting the desk's refusal; a hatch that fails
guest-side enquires `` `agent-abort [token] `` so the desk frees the
reservation. The enquiring builtin cannot tell which arm served it — both
answer the same `` `started [name, log-dir] `` receipt. See
[[design/agents|agents]] for the seed's isolation law and
[[map/synod|synod]] for the hatchery's landed implementation (synod's
accept-pump over `Machine::accept_agent`). The `` `mnemon `` memory mode
additionally forks
the parent's `AgentLog` and imports its model-visible context before assembly
([[decisions/260702_subagent-memory-modes|subagent-memory-modes]]); `AgentLog`
drops a pending unanswered assistant tool-call frame when the parent is
mid-dispatch, so the child inherits a request context rather than a dangling
provider protocol. Both memory modes seed the launch prompt through the
child's inbox, so the prompt enters through the same item path.

Routing the fork through core matters because the builtin table is the easiest
thing to drop. The exarch host builtins — `view-text`/`view-hash`, `grep-files`,
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
so the user never sees more of a result than the model does. A structured
`VALUE` now arrives pre-budgeted — the printer spends its own byte budget inside
the value, cutting where a container can still name what it dropped — so `clip`
is a backstop there, while stdout, stderr, and raw payload strings still meet it
head-on. `run_shell` here threads to [[map/exarch/shell-eval|shell-eval]].

## See also

[[design/agents|agents]] (the role model these nodes realise — one `parent`
predicate, the conversing trunk vs returning agents),
[[map/exarch/tools|tools]] (the one `ral` tool the
provider sees) and [[map/exarch/builtins|builtins]] (the harness verbs the
desk answers), [[map/exarch/frontend|frontend]] (the bus, the inbox, the
registry, and the two frontends), [[map/exarch/provider|provider]],
[[map/exarch/policy|policy]], [[map/exarch|exarch]],
[[design/residency|residency]] (the resident ledger this cascade and the
worker/schedule teardown edge are chapters of),
[[decisions/260722_session-is-a-process|session-is-a-process]] (why a wire
seat is one engine process, one connection),
[[map/synod|synod]] (the hatchery, `AGENT_PORT`, and synod's own helper
surface built over the wire-seat spawn above).
