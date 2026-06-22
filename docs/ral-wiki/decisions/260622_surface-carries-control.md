---
status: proposed
---

# `surface` is the host's event channel, not only its presentation channel

**The `surface` sink began as the channel by which the kit hands exarch a render
document, grew to also carry the structural I/O events core emits, and should now
be recognised for what it has become: the single typed `Value` channel from the
language to the host, of which *presentation* is one class among several.** The
fulcrum that forces the recognition is a **control event** — `` `spawn-started ``,
carrying a live `Value::Handle` — that exarch consumes not to draw anything but
to **register the handle and arm a completion waiter that posts to the inbox**.
It binds to no `Card`, serialises to no `events.json` row, and lights no pixel.
Once one surfaced event renders nothing, "surface is presentation" is no longer
the channel's meaning; it is the meaning of one arm of the host's decoder. The
host dispatches surfaced values **by class** — *render* (`` `card ``, `io`) versus
*control* (`` `spawn-started ``) — and only the render classes reach the rail and
the log.

This is the mechanism that lets `spawn` **notify** instead of being **polled**,
without a second worker→host channel and without handing the detached worker the
presentation sender it is forbidden to hold.

## Context

### The surface lineage

`surface <value>` (`core/src/builtins/misc.rs:530`) forwards one `Value` to the
turn-local `EventSink` (`SurfaceSink = Arc<dyn EventSink>`, `core/src/turn.rs`,
[[decisions/260617_turn-local-state|turn-local-state]]). Core names no meaning;
exarch's `AgentSink::emit` (`exarch/src/shell_eval.rs:71`) decides what each value
*is*. Two classes already arrive there, and they already render through two
different decoders:

- a kit-composed **render document** — a `` `card `` `Variant` of Bertin marks,
  decoded by `value_to_card` ([[decisions/260619_surface-carries-documents|surface-carries-documents]]);
- a core-emitted **structural I/O event** — an `io`-tagged `Map` (a read, write,
  exec, grep), decoded into an `IoEvent` and *paired with* a `Card`
  ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).

So the framing "surface carries presentation" was already strained:
`surface-reads-writes-execs` had core, not the kit, emit onto the channel, under
the slogan *"core names the operation, exarch names its appearance."* But both
classes still terminate in a `Card` on the rail and a structural row in
`events.json`. The channel's *output* was still, uniformly, presentation.

### The problem that breaks it: notify, don't poll

A `spawn` worker is detached — root-parented, surviving the turn, reaped at the
one-hour ceiling ([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]).
Its result is delivered **only by pull**: the model must `await`/`poll`/`race`
the `Value::Handle`. The canonical idiom that ADR documents is literally a poll
loop — *"`spawn` → `poll` across turns → `await` once settled"* — and exarch's
own inline-timeout error instructs it (`shell_eval.rs:165`): *"Spawn it … then
`poll $h` on later turns."* The harness **tells the model to poll**, so it polls;
others forget to `await` and let the turn end; others assume a notification that
never comes.

The fix the model keeps reaching for is the right one — *be notified* — and
exarch already owns the rail for it: the **inbox** (`InboxMsg`, session-lived,
drained at the turn boundary), on which the async `agent` posts `AgentResult` and
which surfaces as a marked `Turn::Agent` next turn
([[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]).
This is the very thing that ADR left open: *"Surfacing from a background child …
unexplored, since muting currently forecloses it."* Two constraints, set by the
author, shaped the answer:

1. **One channel.** No second `EventSink` beside `surface`. A parallel
   `detached_sink` was proposed and rejected.
2. **The detached worker gets no host channel.** The standing invariant —
   *detachment may hold only root- or handle-owned resources, never a foreground
   frame's capture state* ([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]],
   [[decisions/260617_watch-repl-builtin|watch-repl-builtin]]) — is precisely why
   the async `agent` worker may **not** hold the foreground turn's `Emitter`
   (`shell_eval.rs:63`: *"a clone of the bus `Emitter` can never outlive the tool
   turn"*). The withholding guards **presentation placement**: a late card from a
   detached worker would splice into root's sealed scrollback. (`emit` is a silent
   `let _ = tx.send` and the TUI bus is session-lived, so it is not a liveness
   hazard — it is an ordering one.)

Together these nearly contradict: a post-turn notification needs *some*
session-lived path the producer can reach, yet the producer may hold none, and we
may not add a channel. The resolution is to notice that **exarch never sees a
`spawn` at all** — it runs a whole ral *script* through `run_shell` and gets back
rendered text; individual `spawn` calls never cross the boundary. So for exarch to
know a spawn exists, the language must *surface* it — and the one channel for "the
language tells the host something structured" is `surface`, used **in-turn**,
where it is alive and correctly placed.

## Decision

### `surface` carries event *classes*; the host dispatches on class

`AgentSink::emit` gains a third arm, and with it a stated shape: a surfaced value
is one of

| class | example | what the host does | renders? | in `events.json`? |
|---|---|---|---|---|
| **document** | `` `card `` | `value_to_card` → `Kind::Card` | yes | yes (mark tree) |
| **I/O** | `{io: read, …}` | `IoEvent` + `Card` → `Kind::Io` | yes | yes (raw event) |
| **control** | `` `spawn-started `` | register handle, arm a waiter | **no** | **no** |

The render classes are unchanged. The new control class is the one that earns the
recharacterisation: it is consumed for its **effect on the host**, not its
appearance. An unknown class still degrades gracefully (drop / plain ink), exactly
as today.

### `spawn` surfaces a control event, in-turn

`spawn` takes a label — the model writes `spawn "build" { … }` — through a
labelled spawn path modelled on the one `watch` uses (`spawn_labelled`,
`concurrency.rs:250`; `watch` routes its label into `ChildIoMode::Watch` for
stdout framing and leaves `cmd` fixed, so `spawn`'s path is a sibling, not a
straight reuse). The label becomes the handle's `cmd` — it reads back as
`<handle:build>` (`value.rs`) — and is the single source the `` `spawn-started ``
payload and the later `SpawnResult` both read, never a second copy. At spawn
time — synchronously, in the foreground turn, while `shell.turn.surface` is the
live `AgentSink` — `builtin_spawn` emits

```
`spawn-started [label: "build", handle: <the Value::Handle>]
```

through that sink. The handle is an ordinary public `Value` variant
(`Value::Handle`), not core's private representation
([[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]),
so passing it breaches no boundary. **The detached worker's closure is
unchanged** — it captures nothing new and surfaces nothing post-turn. Core grows
no host vocabulary: it emits a `Value`, as `surface-reads-writes-execs` already
has it do, and names no notion of "inbox" or "waiter."

### exarch adopts the handle and waits off-thread

On `` `spawn-started ``, `AgentSink` extracts the `HandleInner`, records it in a
session-lived spawn registry (a sibling of `AgentRegistry`), and starts a
**waiter** — structurally the async-`agent` worker minus the child `Session`. The
waiter holds only the **handle** (handle-owned) and the **inbox** (session-owned,
built for exactly this — `bus.rs:529`); it holds **no** foreground `Emitter`. It
blocks on the handle's completion, then — *unless an in-script `await` has already
claimed the handle* — drains the settled record and posts

```
inbox.push(InboxMsg::SpawnResult { label, outcome, text, surfaced })
```

`drain_turn` turns it into `Turn::Spawn`; `marked_turn` decodes the `surfaced`
values exactly as the live surface would (`value_to_io`/`value_to_card`), then
renders `[spawn 'build' finished]`, and it lands as the next turn with no user
input — the agent rail verbatim, one new variant. `/clear` cancels registered
handles and the late `SpawnResult` is generation-gated out, exactly as a stale
`AgentResult` is (`agent_registry.rs:159`).

**Deliver once.** A handle now has two possible observers — the model's in-script
`await` and the waiter — but exactly one *delivers*. A `delivered` latch on the
handle (folding in today's `surface_replayed` flag) is claimed by whichever path
settles first. If the model `await`s, it replays `surface_buf` into the awaiting
turn and renders as it does today, and claims the latch — so the waiter, on
waking, finds the handle delivered and posts **nothing** (no redundant `[spawn
'build' finished]`). If the model never `await`s, the waiter is the deliverer: it
carries the buffered `surfaced` values and the text on the `SpawnResult`, and the
notification turn renders them at the boundary — deferred, never live, so the
terminal lease is uncontested. A later stray `await` finds the latch set, returns
the §13.3 cached value, and replays nothing. This is what keeps a `surface`
*inside* a spawned body from being stranded once the contract stops telling the
model to `await`.

### The model-facing contract changes

The canonical idiom is no longer *poll across turns*. `spawn` returns immediately
and runs in the background; the host **notifies** when it finishes. The system
prompt (`exarch/data/ral.md`) and the inline-timeout error (`shell_eval.rs:165`)
are rewritten: don't poll; don't `await` unless you need the value in *this* same
script; label the spawn so the notice is legible. `await`/`poll`/`race` remain —
they are how an in-script join works and how a value is pulled on demand — but
they are no longer the *cross-turn observation* mechanism. This **supersedes** the
cross-turn observation idiom of [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
— which promoted `poll` to the *primary* cross-turn observation — while leaving
the handle/detachment model itself intact.

## Safety — why this respects the withheld channel

The author's caution was correct, and an earlier draft of this design was unsafe:
it gave the detached worker the `AgentSink` and trusted it to call only
`.inbox()`, never `.emit()`. That relies on *discipline* inside a worker holding
the whole `Emitter`; one stray presentation emit re-creates the late-card bug.
"Never outlive the turn" is the *enforcement*, not a softenable comment.

This design never hands the worker that channel:

- **The control event is emitted in-turn**, by `builtin_spawn`, through the
  live foreground surface. Nothing crosses the turn boundary on the producer
  side. Placement is correct because the turn is still active.
- **The detached worker holds only what it always held** — its root-parented
  scope and its own buffers. Its closure is byte-for-byte unchanged, so the
  invariant is upheld *by construction*, not by review.
- **The waiter is exarch's, not the worker's**, and holds only handle-owned and
  session-owned resources — the same pair the async `agent` worker is already
  permitted (`tools/agent.rs`: it captures `inbox`, never the foreground
  emitter). It posts to the inbox, the rail purpose-built to outlive a turn;
  carrying the buffered cards on that message is moving *data* onto the deferred
  rail, not a render call. They render at the next turn boundary, appended as a
  fresh turn — never spliced into root's sealed scrollback, and never live, so the
  terminal lease stays uncontested.

The two delivery legs use the two existing channels for what each is *for*:
**surface in-turn** to register (presentation-class machinery, used while live),
**inbox post-turn** to notify (the deferred rail). No third channel; the worker
gets none.

## Why this shape

- **It is the next step on a road already taken.** `surface-carries-documents`
  opened the card set; `surface-reads-writes-execs` put core on the channel and
  separated *operation* from *appearance*. This separates *render* from *control* —
  the same move one level up. The channel was never really "presentation"; it was
  "structured values from the language," and presentation was the only consumer so
  far.
- **It dissolves the contradiction without new machinery.** One channel, used
  in-turn; the worker holds nothing; the notification rides the inbox that already
  exists. The alternative each violated one constraint; this violates none.
- **It deletes the instruction that caused the behaviour.** The poll loop was not
  a model failing — the harness prescribed it. Replacing the prescription with a
  notification is the smallest honest fix.
- **It keeps `events.json` truthful.** Control events carry live runtime objects
  (a handle), so they *cannot* serialise — and they must not. Dispatch-by-class
  routes them away from the log, which records only the render classes, as before.
  The log stays a faithful trace of what was *shown*, not of host bookkeeping.

## New core primitive, and the one real risk

The waiter has no `Shell`, so it cannot call `await_handle` (which polls
`process::check` in the foreground). Core gains a **shell-free blocking settle**
on the handle — block on the result `Receiver`, drain it into the
`CompletedHandle` cache, and return the settled record *plus the buffered
`surface_buf` values* (public ral `Value`s, so nothing of core's repr leaks). The
model's `await` path is untouched; the new settle is the waiter's equivalent, and
`surface_buf`/`surface_replayed` fold into the `delivered` latch so the buffer is
replayed-or-carried exactly once.

The genuine subtlety: today a handle has exactly **one** observer (the model). This
adds a **second** (the waiter), and the `delivered` latch sits in the same
critical section. The `Mutex<Option<Receiver>>` → `Mutex<Option<CompletedHandle>>`
handoff must be atomic for "whoever settles first wins, the other reads the cache
and does not re-deliver" — including the window where the receiver has been taken
but the cache and latch are not yet populated. This is the one place that needs
real concurrency care; everything else reuses settled machinery.

Cancellation rides the same edge: `/clear` cancels the handle, dropping the
worker's result sender, so the waiter's blocking settle returns `Disconnected`,
wakes, and its late post is generation-gated out — no leaked thread.

## Alternatives considered

- **A second `EventSink` (`detached_sink`) beside `surface`.** Rejected by the
  one-channel constraint: it is literally a parallel pipe to the host, and
  `surface` already carries non-presentation values, so the second channel earns
  nothing the third decoder arm does not.
- **Give the detached worker the `AgentSink` (emit `` `spawn-done `` post-turn),
  disciplined to inbox-only.** Rejected as unsafe: it hands the worker the very
  channel that must not outlive the turn and depends on it never touching the
  presentation sender. The enforcement is structural withholding, not a rule the
  worker promises to follow.
- **A worker-side completion callback in core (`Turn` gains a notify hook).**
  Rejected: it teaches core a host notion (an out-of-band sink that outlives the
  turn) and re-introduces a producer-held channel under another name — the same
  two objections, relocated.
- **exarch polls registered handles at idle** instead of a waiter thread.
  Workable (it is host-internal, not model-visible polling) and thread-free, but
  it reintroduces polling cadence where a blocking waiter mirrors the agent rail
  exactly. Kept in reserve; not the primary shape.
- **A model-facing introspection primitive** (`jobs` for handles). Out of scope
  and against [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]:
  detached workers are unmanaged *to the model*. The spawn registry is host
  bookkeeping, invisible to the model, exactly as the agent registry is.

## Consequences

- `surface` is documented as the language→host typed-`Value` channel; "presentation"
  names the render classes, not the channel. Adding a future host-side effect
  (another control event) is one decoder arm, no new channel.
- `spawn` notifies. The poll idiom and the poll-instructing timeout text retire.
- The detachment invariant is preserved by construction; the unsafe variant is on
  the record as rejected.
- `events.json` and the rail still see only render classes; control events are
  invisible to both, honestly (they cannot serialise).
- Core gains one public shell-free settle; the two-observer cache handoff is the
  sole concurrency risk.
- The spawn registry + waiter is new host bookkeeping, sibling to `AgentRegistry`,
  generation-gated by the same `/clear` discipline.

## Open questions

- **Nested `spawn`.** A `spawn` *inside* a detached worker runs under the worker's
  `TurnState`, whose surface is the `DeferredSurface` (buffered, replayed on
  `await`), not the live foreground sink. Its `` `spawn-started `` would therefore
  reach exarch only on the parent's `await` — by which time the inner worker may
  already have finished. Nested detached spawn is rare (and arguably wants an
  `agent`), but the degraded notification timing is on the record. Whether to lift
  control events out of the deferred-replay path is unresolved.
- **Surfacing a *card* from a *live* background child.** Still the open question of
  [[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]] — a
  worker that raises a render document *mid-flight* competes for the foreground and
  the terminal lease ([[decisions/260619_terminal-lease|terminal-lease]]), and that
  remains foreclosed. The narrower in-body case is *not* open: a `surface` inside a
  spawned body buffers into `surface_buf` and is delivered — rendered at the
  boundary — by whichever of the model's `await` or the waiter settles first
  (deliver-once, above). Only *live* render competition is unsolved.
- **Promotion to a durable job.** The long-running-work question
  ([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]])
  is untouched; a notified `spawn` is still ceiling-bound. A handle the host
  notifies on is a natural building block for an eventual promote/disown, but that
  mechanism is not this decision.

## See also

[[decisions/260619_surface-carries-documents|surface-carries-documents]] (the
render-document class, still true for its arm),
[[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]] (core
on the channel; *operation* vs *appearance* — this extends it to *render* vs
*control*),
[[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]] (the
bus/inbox split and the background-surfacing open question this partly answers),
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the handle/detachment model and the poll idiom this refines),
[[decisions/260617_turn-local-state|turn-local-state]] (`surface` is a turn-local
field — why the control event is emitted in-turn),
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (the
detachment-holds-only-root/handle-resources invariant),
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
(the surfaced handle is a public `Value`, not core's repr),
[[map/exarch/shell-eval|shell-eval]] (the `AgentSink`/`value_to_card` seam the
third arm joins), [[map/exarch/cards|cards]] (the render subsystem),
[[map/core/builtins|map: builtins]] (`spawn` and the concurrency primitives), and
`docs/SPEC.md` §13.3 (the handle settle/replay rule the waiter must not disturb).
