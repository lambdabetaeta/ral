---
status: active
---

# `agent` has sync and async modes

**The exarch `agent` tool has two modes because a sub-agent call may be a
*dependency edge* or an *orchestration edge*.** `sync` returns the child's answer
in the current `tool_result`; `async` returns an opaque start receipt now and
later posts a typed `AgentResult` to the per-session inbox. Async is not a ral
handle and not a durable job: it is an ephemeral, session-owned agent worker
with an id for status and cancellation.

## Context

The current sync `agent` tool is fork-join, not background work. Same-batch
children may overlap under `Session::dispatch`'s `thread::scope`, but dispatch
joins every `Staged::Spawned` before the next provider request. That is correct
for dependency edges and useful for batch fan-out, but it still cannot express
the parent turn ending while the child continues.

Two facts bound the space:

- **The wire constraint is inescapable.** A `tool_use` block must be answered by
  its `tool_result` in the next message, so an async tool cannot defer its
  result: it returns a stub synchronously and its real reply must re-enter the
  loop later. The turn still exits `ReadyForUser`
  ([[invariants/turn-ends-ready|turn-ends-ready]]) however the stub returns.
- **The one thing in-turn concurrency cannot express** is the parent turn
  *ending before the child does*. Running a batch of agents at once is a
  wall-clock win on a single turn; it is not the same as a sub-agent doing work
  *off the parent's critical path* — spawned, then left running while the parent
  continues, its reply arriving when ready. That second shape is what an async
  agent is for, and it is the only shape that genuinely needs the child to
  outlive its turn.

The mechanics of a turn-outliving detached worker already exist for ral `spawn`
([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]):
parented at the durable root so a foreground deadline cannot collaterally kill
it, reaped by the shared `process::reaper` death-clock, carrying its own cancel
scope. What ral `spawn` *cannot* host is an LLM turn — its worker body is ral
code, which lives below the Provider. An async agent is that same detached worker
with a *turn loop* for a body, which is precisely why it is born on the exarch
side, as a tool, rather than expressed in the language.

## Decision

- **`agent` is bimodal.** `mode: "sync"` means "run this child because I need
  its answer now"; `mode: "async"` means "start this child because I am
  orchestrating independent work". Neither edge subsumes the other. Sync stays
  the compatibility default because today's `agent` result is the child's final
  answer, not a start receipt.

- **Sync keeps the existing virtues.** A sync child returns in the current
  `tool_result`: one model round-trip, deterministic placement at the call site,
  live child chrome, and no lifetime surface because the child cannot outlive the
  call. This is the right shape when the parent turn depends on the answer before
  it can continue.

- **Async returns a start receipt, not the answer.** The immediate `tool_result`
  satisfies the wire protocol and carries `{id, title, status: "started",
  log_dir}`. The `id` is an opaque `AgentId`, not a content hash and not prose in
  a tag. It is the capability for later status and cancellation.

- **The child escapes the dispatch scope.** Today it is a `ScopedJoinHandle` on
  the `thread::scope` in `Session::dispatch`; an async child is a real worker
  owned by a session-lived agent registry. It survives the spawning turn and is
  not joined before the next tool boundary.

- **The registry is ephemeral, session-owned, and listable while useful.** Async
  is not [[decisions/260617_long-running-work|long-running-work]]'s durable-job
  registry, but orchestration still needs recovery from context compaction and a
  way to stop stragglers. `agents` lists live async agents by id, title, status,
  log directory, and elapsed time; `agent_cancel(id)` cancels one. Settled
  results auto-deliver through the inbox, so there is no required
  `agent_collect(id)` path.

- **Provider ownership becomes shared, but cancellation becomes request-local.**
  A worker outliving the dispatch frame cannot hold the borrowed `&'env
  Provider`; the driver-held provider becomes an `Arc<Provider>`, and an async
  child captures the current clone. A later `/model` switch replaces the driver's
  current `Arc` but does not rewrite already-running children. `Arc<Provider>` is
  necessary but not sufficient: `Provider::complete` / `summarize` must also take
  a request cancellation handle rather than relying only on the process-global
  root-turn slot (`cancel::is_set`). The foreground turn's handle is linked to
  the signal slot; an async child receives the registry's handle, so two
  concurrent provider requests do not overwrite each other's cancellation path.

- **The reply re-enters through a typed inbox message.** The child posts
  `InboxMsg::AgentResult { id, title, outcome, text, log_dir, elapsed,
  generation }` at settle. The driver renders that host data as a synthetic user
  turn at the next turn boundary. It is not raw `<agent=...>` text in the prompt
  queue: source tags and drain boundaries are data, not parsed prose.

- **Output is muted to the child's own log.** The event bus is per-turn and
  closes when the spawning turn ends, so a background child cannot stream to it —
  the same constraint that denies exarch `watch` (a detached writer may hold only
  root- or handle-owned resources, never a foreground frame's capture buffers).
  The child writes to its own `SessionLog`; its final reply surfaces through the
  inbox.

- **Lifetime is explicit registry policy.** ral `spawn` inherits
  `detached_ceiling` through `TurnFrame`; an exarch-side agent worker does not.
  The async-agent registry therefore owns a worker cancel scope, arms the shared
  `process::reaper` with the same detached-worker ceiling, and drops the entry
  when the worker settles. `Session::clear` cancels all live async agents,
  increments a generation, and rejects any late `AgentResult` from an older
  generation. Process exit drops the registry and root-aborts remaining children.

- **No ral surface.** This is an exarch tool mode, not the born-durable ral verb
  of [[decisions/260617_long-running-work|long-running-work]]. The two share the
  detached-worker vocabulary but not their management surface: the durable-work
  path is a ral `Handle` / durable job with `poll` / `await` / `cancel`; async
  `agent` is a session-owned LLM worker whose result is pushed to the inbox.

  > **Landed 2026-07-16 — superseded in letter, not in substance.** The
  > launch verbs (`amnemon`, `mnemon`, and the rest of the harness family)
  > are now `ral` builtins the model reaches by writing ral inside the one
  > remaining tool ([[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]),
  > so "No ral surface" no longer holds literally. The distinction this
  > bullet protects survives the move: a builtin is exarch-side, installed
  > above ral-core, and answered by a host desk (`shell.enquire`) — it is
  > still not the ral-core `Handle`/`poll`/`await`/`cancel` verb this ADR
  > and [[decisions/260617_long-running-work|long-running-work]] rejected,
  > and the worker body is still an LLM turn that cannot live in the
  > language. See [[map/exarch/builtins|builtins]] for the landed verb and
  > [[map/exarch/tools|tools]] for what remains a tool.

## Why this shape

- **It preserves the distinction the model needs to express.** Sync is a
  dependency edge; async is an orchestration edge. Keeping every call sync leaves
  orchestration on the parent turn's critical path. Making every call async turns
  ordinary dependency calls into a two-turn protocol with a deferred result.
- **It honours the wire constraint without making the model carry collection
  debt.** A stub now, a pushed reply later; the harness owns the obligation to
  deliver the result. The id exists for status and cancellation, not as the
  required way to collect the answer.
- **It composes with [[decisions/260617_scheduled-wakeups|scheduled-wakeups]]:**
  a worker reply and a cron prompt are both `InboxMsg`s with source tags,
  delivered at the turn boundary — distinct from
  [[decisions/260616_tool-boundary-steering|tool-boundary-steering]], which is
  *user* redirection drained mid-turn. Building the inbox once serves both.
- **It names the real new mechanisms.** The reuse is detached-worker parenting
  and the reaper, the inbox delivery seam, and `Session::fork`'s snapshot model.
  The new code is provider `Arc` ownership, request-local provider cancellation,
  an off-scope spawn site, an ephemeral agent registry, explicit reaper arming,
  and stale-generation rejection on `/clear`.

## Alternatives considered

- **A ral `Handle` (`poll` / `await` / `cancel`), the
  [[decisions/260617_long-running-work|long-running-work]] Regime-1 shape.**
  Rejected for this need: the affordance is an LLM tool mode, not a language
  construct, and the desired fan-in is push delivery. Async agents still have an
  id for status and cancellation, but a pushed result means the model does not
  have to retain that id to receive the answer.
- **No model-facing id.** Rejected: pure auto-notify is enough only when every
  worker succeeds quickly. An orchestrator needs to inspect live children, cancel
  stragglers, and recover ids after compaction; the registry is therefore
  listable while entries are live.
- **An explicit `agent_collect(id)` as the primary path.** Rejected: it makes the
  model carry a job id across turns and compaction merely to receive the answer.
  A deterministic collection verb can be added later if a workflow needs it, but
  inbox auto-delivery is the default fan-in.
- **Make every agent async.** Rejected: it changes the meaning of a dependency
  call. The parent receives "started" instead of the answer, pays another model
  turn, and must reason about a later synthetic message even when it had no work
  to interleave.
- **A separate `agent_spawn` tool.** Rejected for now: the two modes are the same
  affordance with a different dependency edge. A required `mode` field in new
  prompts makes the edge explicit without splitting docs, permissions, and UI
  chrome across two tool definitions.
- **Raw tag delivery (`<agent=...>...</agent>` in the prompt queue).** Rejected:
  tags are forgeable text and force drain policy, logging, and UI state to parse
  prose. The queue becomes a typed inbox; rendering to text happens only at the
  model boundary.
- **Lift the bus for live background output now.** Deferred: muting matches the
  existing `watch` denial and keeps the bus per-turn; live streaming from a
  background child is a larger, later change.

## Open questions

- **Mute vs live.** Whether a background agent ever streams live (lifting the bus
  to session lifetime) or stays muted-to-log and surfaces only on inbox delivery.
- **The ceiling for an LLM worker.** One hour is the detached-worker default; a
  background agent doing genuinely long work may need a different bound — which
  is [[decisions/260617_long-running-work|long-running-work]]'s open regime
  question surfacing again, here for a tool rather than a ral verb.
- **Delivery batching.** If several agents settle before the driver drains the
  inbox, either render one synthetic turn per agent or coalesce them into one
  ordered batch. The message type supports both; the frontend policy is open.
- **Post-delivery retention.** How long a settled registry entry remains
  listable after its result has been delivered — enough for UI/log discovery,
  not a durable job history.
- **Dependence on the inbox.** Delivery rests on the
  [[decisions/260617_scheduled-wakeups|scheduled-wakeups]] inbox seam; if that is
  not built first, this ADR builds the inbox itself, for the same two consumers.

## See also

[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the detached-worker mechanics this reuses: root parenting, the reaper, the
ceiling), [[decisions/260617_scheduled-wakeups|scheduled-wakeups]] (the inbox
delivery seam and the event-trigger this realises),
[[decisions/260617_long-running-work|long-running-work]] (the ral born-durable
verb this is deliberately *not*),
[[decisions/260523_background-tool-calls|background-tool-calls]] (the earlier
study that mapped provider sharing and a registry),
[[decisions/260616_tool-boundary-steering|tool-boundary-steering]] (why sync
agents are still fork-join, and why inbox delivery is not steering),
[[invariants/turn-ends-ready|turn-ends-ready]], [[map/exarch/agent|agent]],
[[map/exarch/tools|tools]], and `docs/SPEC.md` §13.
