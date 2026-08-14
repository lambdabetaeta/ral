---
status: superseded
---

# Long exarch sessions need explicit resource budgets

> Superseded by leases-and-budgets,
> which carries every per-accumulator decision here forward and adds the probe
> convention (`/resources` as a fold over registered budget probes), the
> worker registry as the accounting spine and completion-payload home, and a
> per-agent admission cap on live workers.

**A days-long exarch run is not bounded by model compaction; it is bounded by
ordinary heap, queues, shell values, and log files.** The provider context may be
kept small while the host still grows without limit. Long-session stability is
therefore a host-resource policy: bounded presentation, bounded delivery,
observable shell residency, and durable logs that do not masquerade as live
memory.

## Context

The exarch core is deliberately session-lived. One [[map/exarch/agent|Agent]]
owns a persistent [[map/core/shell-state|Shell]], a persistent log, an inbox, and
a provider loop; the [[map/exarch/frontend|frontend]] owns session-lived viewports
and a session-lived event bus. That is the right shape for continuity, but every
session-lived thing must say what bounds it.

The current pressure points are distinct:

- **The TUI keeps a live graphic transcript.** `Viewport` stores blocks plus a
  flattened wrapped-row cache; `Tabs` retains viewports for dead agents so their
  logs can be flushed. A long run with many sub-agents, cards, diffs, and tool
  panes can grow in memory even when the visible tab strip is small.
- **The bus is presentation, but still heap.** The session-lifetime bus
  ([[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]])
  uses an unbounded `mpsc` channel. If producers outrun the renderer, queued
  `Kind` events consume memory. Presentation is not control flow, but it can
  still kill the process.
- **The inbox is model-facing delivery.** Per-agent inboxes are unbounded
  `VecDeque`s drained only at tool or turn boundaries. They cannot be treated
  like presentation: silent loss changes what the model sees.
- **Compaction is not reclamation.** *(Superseded for the event mirror,
  260812: the edit step now frees a span's events the moment it leaves the
  view — [[decisions/260812_context-is-a-projection|context-is-a-projection]] —
  so the mirror is O(view). The files still retain everything.)*
  `events.jsonl` (since unified into `record.jsonl`) and `user.log` are
  durable records, not memory budgets; `transcript.jsonl` was a third such
  record and is since deleted outright
  (`dev/docs/plans/260814_kind_dissolves.md`), not folded into a budget.
- **The lexical env is cheap to clone, not cheap to fill.** `Env` is persistent
  copy-on-write, so a big env is not a clone bomb. The resident cost is the
  `Value`: large strings, bytes, lists, maps, closures that captured old scopes,
  and `Handle`s retaining buffered stdout/stderr and cached results.
- **Some buffers are already bounded, multiplicatively.** Captured byte sinks
  have a 16 MiB cap and detached surface buffers have an event cap, but many
  handles or agents can still add up. The one-hour exarch detached-worker ceiling
  limits forgotten workers, not settled handles kept in bindings.

So the failure mode over days is not one large thing; it is many small
session-lived accumulators with no common accounting.

## Decision

Exarch should make long-session residency explicit and observable before it is
allowed to become unbounded.

- **Separate context budget from host budget.** `/compact` remains a provider
  context operation. It must not be sold as memory reclamation. Heap and disk
  limits get their own machinery and diagnostics.

- **Bound the presentation bus by class.** Replace the unbounded bus with a
  bounded/coalescing transport. Lifecycle events (`Born`, `Died`, terminal
  tool-result frames) are reserved; token, phase, progress, and high-frequency
  card updates may be coalesced. If a class is elided, the viewport receives one
  explicit overflow marker naming the dropped class and count. The user sees
  degradation, not silence.

- **Keep only a viewport window in memory.** A viewport owns the current reply,
  pins, and the last `N` blocks/rows/bytes; older blocks are already durable in
  `user.log`/`events.jsonl` and should not be duplicated forever. Dead sub-agent
  viewports are flushed, then evicted after linger into a tombstone carrying id,
  status, and the log path. `/clear` stays the explicit whole-subtree reset.

- **Make compaction physically drop the model prefix in memory.** *(Delivered
  260812, generalized to every context edit — drops reclaim too:
  [[decisions/260812_context-is-a-projection|context-is-a-projection]].)* The
  in-memory ledger keeps exactly the view; the append-only `events.jsonl`
  remains the forensic record. Reclamation is for heap, not history.

- **Bound inboxes without silent loss.** The inbox carries model-facing facts, so
  it needs quotas and source policies rather than blind dropping. Idempotent
  sources may coalesce (`schedule` wakeups by id, repeated steering before a
  turn boundary, duplicate completion nudges); non-idempotent facts must either
  be accepted or rejected with a user-facing error such as "this agent's inbox is
  full; cancel work, focus it, or clear the subtree?" Completion payloads should
  live in registries when possible, with the inbox carrying a bounded wakeup.

- **Treat shell residency as lexical state plus host leases.** ral bindings do
  not expire by language rule. Exarch may apply
  [[decisions/260629_agent-binding-reaping|agent-binding-reaping]] to scratch
  top-level names, warn on large bindings, and prefer paths over captured bytes.
  Running handles pin names; settled handles are scratch. A closure capture may
  keep a value alive after its top-level name is pruned, because that is lexical
  scope, not a leak detector.

- **Give the operator a resource view.** Add a `/resources` or `/memory`
  diagnostic that reports, per fleet and per agent: bus depth, inbox depth,
  viewport block/row/byte counts, event-vector length, live/dead viewports,
  shell binding count, large bindings by rough retained size, running/settled
  handles, and log/scratch disk use. A budget that cannot be inspected will be
  debugged by restarting the process.

- **Keep days-long computation outside ordinary `spawn`.** Work expected to
  outlive the detached-worker ceiling belongs in
  [[decisions/260617_long-running-work|long-running-work]]'s born-durable job
  regime or in an external process with logs. Ordinary `spawn` remains a bounded
  session construct.

## Suggested defaults

The first implementation should choose conservative defaults and make them
visible in `/resources`, not buried as constants.

- **Viewport:** cap by both blocks and rendered rows; evict old blocks before
  retaining old dead-agent views.
- **Bus:** reserve capacity for lifecycle/final frames; coalesce stream tokens
  and progress updates per agent.
- **Inbox:** cap per agent and per source; store large completion material out of
  band by id; reject rather than silently drop non-idempotent messages.
- **Events:** after compaction, drain the old in-memory prefix and keep the
  summary plus suffix; leave append-only files alone.
- **Shell:** warn before binding values above a soft threshold; recommend file
  paths for large outputs; reap settled scratch handles by idle call count.
- **Disk:** report session-log and scratch size; warn before the host reaches an
  operator-set ceiling.

## Consequences

- **Overflow becomes a UI state.** The TUI can say "some presentation was
  elided" and keep running. This is better than preserving every token until the
  host dies.
- **Model-facing delivery stays honest.** Inbox pressure is surfaced as a
  rejection or a coalesced registry wakeup, never as invisible loss.
- **Headless and TUI diverge only in presentation budget.** Headless has no
  viewport pressure but still has logs, event vectors, inboxes, shell bindings,
  and handles.
- **Memory reclamation is intentionally incomplete.** Pruning a top-level name or
  old event prefix may not free a captured value. The diagnostic should explain
  reachability rather than promise exact retained size.
- **Long sessions become inspectable.** The operator can decide to compact,
  clear, cancel a subtree, reap scratch, rotate logs, or restart with evidence.

## Alternatives considered

- **Rely on `/compact` and `/clear`.** Rejected. `/compact` is a model-context
  operation, and `/clear` is a destructive reset. Neither is a resource policy.

- **Let the OS be the budget.** Rejected. Process death loses live shell state,
  handles, schedules, inbox messages, and focus. Durable logs help inspection but
  do not resume the heap.

- **Auto-clear old state.** Rejected. Clearing changes shell and agent semantics.
  Presentation can be evicted; model-facing and lexical state need explicit
  policies.

- **Keep the bus unbounded because it is only presentation.** Rejected.
  Presentation is non-authoritative, but unbounded presentation can still exhaust
  memory and prevent the authoritative loop from making progress.

## Open questions

- What are the first concrete default caps for blocks, rows, events, bus depth,
  inbox depth, and disk warning size?
- Should old viewport blocks be reloadable from `user.log`, or is a tombstone
  plus log path enough?
- Should disk rotation happen during a live session, or only at session start and
  session close?
- How much retained-size estimation is worth building for `Value` graphs with
  `Arc<Env>` captures and shared handles?
- Should the future transport-parametric frontend
  ([[decisions/260628_host-seam-transport-parametric|host-seam-transport-parametric]])
  negotiate these budgets over the frontend/engine boundary?

## See also

[[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]
(the session-lived bus whose lifetime makes a budget necessary),
[[decisions/260629_agent-binding-reaping|agent-binding-reaping]] (scratch-name
reclamation for agent shells),
[[decisions/260617_long-running-work|long-running-work]] (the durable-job shape
for computations that outlive ordinary `spawn`),
[[decisions/260617_scheduled-wakeups|scheduled-wakeups]] (inbox delivery and
timer pressure), [[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]]
(the TUI transcript model),
[[map/exarch/frontend|frontend]], [[map/exarch/agent|agent]],
[[map/core/shell-state|shell-state]], and
[[internals/output-capture-and-detachment|output-capture-and-detachment]].
