---
status: superseded
---

# Two language→host channels, split by lifetime: `surface` (sync) and `notify` (async)

**There are exactly two channels from the language to the host, and the axis that
separates them is the one property a value's tag can never carry: *lifetime*.
`surface` is the **synchronous** channel — in-turn, alive, holding the foreground
lease, able to render *now*. Its sibling `notify` is the **asynchronous** channel
— post-turn, deferred, rendered at the next turn boundary as a fresh turn. Both
carry a typed `Value`; both dispatch *by class*. So a new *kind* of event is a new
decoder arm on whichever channel's lifetime fits it — never a new channel.**
`spawn`'s completion is the first async event: the detached worker, at the instant
it settles, emits its result through `notify`; exarch's installed `notify` sink
pushes it onto the inbox, and it lands next turn as `[spawn 'build' finished]`.
No control class, no waiter thread, no shell-free settle, no two-observer handoff.

This is the counter-proposal to its sibling
[[decisions/260622_surface-carries-control|surface-carries-control]], which solves
the same problem (`spawn` should notify, not be polled) by routing a *post-turn*
need through the *in-turn* channel. Both are `proposed`; this page argues the
split-by-lifetime shape is the cheaper fixed point and sets it against the
one-channel-plus-control-class shape so the author can choose.

## Context

### The surface lineage, and the axis it never named

`surface <value>` (`core/src/builtins/misc.rs:530`) hands one `Value` to the
turn-local sink (`shell.turn.surface`, an `Arc<dyn EventSink>`,
[[decisions/260617_turn-local-state|turn-local-state]]). Core names no meaning;
exarch's `AgentSink::emit` (`exarch/src/shell_eval.rs:71`) decodes each value *by
class* — an `io` `Map` → `Kind::Io`, a `` `card `` `Variant` → `Kind::Card`,
anything else dropped ([[decisions/260619_surface-carries-documents|surface-carries-documents]],
[[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]). The
channel was already "typed values from the language, dispatched by class"; the
sibling ADR's recharacterisation is correct and this page keeps it.

But every class that has ever ridden `surface` shares an unstated property: it is
emitted **in-turn**, while the turn is alive and the sink is the live foreground
`Emitter`. That property is not in any tag. It is a fact about *when the producer
was permitted to reach the host*, fixed before any value exists. It is the axis
the lineage never had to name — because, until `spawn`, there was only one
lifetime.

### The problem that introduces a second lifetime: notify, don't poll

A `spawn` worker is detached — root-parented, surviving the turn, reaped at the
one-hour ceiling ([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]).
Its result is delivered **only by pull**, and the harness *tells the model to
poll*: the inline-timeout error literally instructs *"Spawn it … then `poll $h` on
later turns"* (`exarch/src/shell_eval.rs:165`). So the model polls; others forget
to `await`; others assume a notification that never comes. The fix the model keeps
reaching for is the right one — *be notified* — and the rail for it already
exists: the **inbox** (session-lived, drained at the turn boundary), on which the
async `agent` already posts its result post-turn and which surfaces as a marked
`Turn::Agent` next turn (`exarch/src/tools/agent.rs:287`, `exarch/src/bus.rs`,
[[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]).

So the notification a completed `spawn` wants is *a value the producer delivers
after its turn has ended.* That is a second lifetime — and it is the whole of the
difference.

### Why the sibling needs machinery

[[decisions/260622_surface-carries-control|surface-carries-control]] keeps a single
channel and meets the post-turn need *through the in-turn one*: `spawn` surfaces a
non-rendering `` `spawn-started `` *control* class carrying a live `Value::Handle`,
in-turn; exarch registers the handle and arms a **waiter thread** that blocks on a
new **shell-free core settle** and posts the completion to the inbox. Because the
handle then has two observers — the model's in-script `await` and the waiter —
both driving the same settle/cache, it needs a `delivered` latch and an atomic
`Mutex<Option<Receiver>>` → `Mutex<Option<CompletedHandle>>` handoff, which that
ADR names as its *sole concurrency risk*.

Every one of those pieces — the non-rendering class, the waiter thread, the
shell-free settle, the two-observer handoff — is the cost of forcing a post-turn
need through the in-turn pipe. None of them is about *spawn*; all of them are about
*reaching the host after the turn through a channel built to reach it during the
turn.*

## Decision

### Two channels, one axis: lifetime

Core's `TurnRequest` carries a second sink beside `surface`, the *same trait*
(`Option<Arc<dyn EventSink>>`, `emit(&Value)`), differing only in lifetime and
wiring:

| | `surface` — **sync** | `notify` — **async** |
|---|---|---|
| lifetime | turn-local; dies with the turn | session-lived; outlives the turn |
| delivery | in-turn, live | post-turn, at the next boundary |
| placement | the foreground frame, now | a fresh turn, like the inbox |
| holds the terminal lease? | yes (it is the live foreground) | no (it never renders live) |
| carries | a typed `Value` | a typed `Value` |
| extends by | a decoder arm (class) | a decoder arm (class) |

Both dispatch *by class*; a new *kind* of event is a new arm on whichever channel's
lifetime fits it. `notify` is, precisely, **the asynchronous `surface`**.

### `notify` is not a new *data* channel — it is the deferred surface's missing trigger

`surface` is already installed two ways, by lifetime. The foreground turn gets
`AgentSink`, which renders live (`exarch/src/shell_eval.rs:119`). A detached worker
gets `DeferredSurface`, which only buffers (`core/src/builtins/concurrency.rs:132`);
those events render later, when someone `await`s the handle. So the worker's surface
is *already* async. It just has no way to deliver itself if no `await` ever comes.

That gap is the whole problem, and `notify` is the missing trigger: at completion,
flush the worker's buffer to the boundary instead of waiting for an `await`. There
is one event vocabulary, dispatched by class, with **two delivery triggers** —
*emit-live* and *push-at-boundary* — chosen by the producer's lifetime. The "second
channel" is really the second trigger. (Two sinks, not one sink with a mode flag,
so a detached worker cannot reach the live trigger by mistake — see Safety.)

This is also why you cannot just make the one channel async. The foreground half is
the live frame — it *is* the thing rendering now. Defer it and a slow tool turn (a
20-second build) goes dark and dumps everything at the end, and a background agent's
tab stops streaming — the muting [[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]
removed. Async is strictly weaker: it can only deliver at a boundary. Sync does
everything except outlive its producer; async does only that. Neither folds into the
other, so the split is forced, not chosen.

### Why lifetime is the one axis that earns a channel

A value's tag tells the host *what the value is*. It cannot tell the host *when the
producer was permitted to reach it* — that is a property of the producer's standing
relative to the turn boundary, settled before the value is constructed. In-turn vs
post-turn is therefore the one distinction that *must* be structural; everything
else — what a value means, how it renders — is dispatch-by-class.

This pins the channel count exactly. Collapse the axis onto one channel and you pay
in machinery to reconstruct it (the sibling: a control class that *triggers*
post-turn delivery, plus the waiter that performs it). Let the channel count track
*purpose* and it explodes (a `detached_sink` today, a third pipe tomorrow). Split on
lifetime — and only lifetime — and the count is two, forever: a new purpose is a new
class, a new lifetime is the thing that does not arrive.

### `spawn` emits its completion through `notify`

The detached worker already has exactly one settling point — `let _ = tx.send(result)`
in `spawn_child` (`core/src/builtins/concurrency.rs:145`). Beside it, the worker
builds a *structural* value and emits it through its (cloned, session-lived)
`notify` sink:

```
`spawn-done [label: "build", outcome: `ok, surfaced: [<the buffered surface values>]]
```

Core names the **event** (`spawn-done`); exarch names its **appearance** — the
exact slogan of [[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]],
one lifetime over. Core grows no host vocabulary: it emits a `Value`, as
`surface-reads-writes-execs` already has it do, and learns no notion of "inbox" or
"notification." The value carries **data, not a live handle**
([[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]),
so — unlike the sibling's control event — it serialises honestly; the block's
return value is *not* shipped on the notice (it stays in the handle's cache for a
later in-script `await`, §13.3), keeping the notice light.

### exarch decodes `spawn-done` at the boundary

`notify`'s exarch implementation (`InboxNotify`, capturing the inbox) pushes the
structural value onto the inbox as a sibling of `AgentResult`. `drain_turn` turns
it into a fresh `Turn` (`exarch/src/bus.rs`); the **boundary render** — in the tui
layer, beside the existing `Turn::Agent` arm (`exarch/src/tui.rs`), where card
rendering already lives — decodes the `surfaced` values with the very
`value_to_io`/`value_to_card` the live `surface` uses, prints `[spawn 'build'
finished]`, and lands as the next turn with no user input: the agent rail verbatim,
one new variant. A late notify after `/clear` is generation-gated out exactly as a
stale `AgentResult` is (`exarch/src/agent_registry.rs`).

The decode lives in the tui, not the bus: the bus moves a structural value as
*data*; rendering a render-document is a presentation concern. (This is the one
layering correction this design makes over the sibling, whose prose places the
decode in `marked_turn`.)

### Deliver-once is a boundary check, not an atomic handoff

A handle can still be joined in-script (`await`/`race`/a settled `poll`) *and* fire
a completion notice. We want exactly one delivery. The worker **always** emits the
notice at completion; suppression is a single `joined` flag on the handle, set
under its existing mutex by any in-script observation, and **read at the turn
boundary** — strictly after the turn that could have set it. Because the check runs
post-turn, no two observers ever contend on a settle critical section: the worker
*emits* (a copy of its own result), it is never *observed*; `await`/`poll`/`race`
read the handle's cache exactly as today (`complete_handle`/`cached`,
`concurrency.rs`). The sibling's sole concurrency risk — the atomic two-observer
cache handoff — simply does not arise.

In-body surfaced cards ride the same edge without contention. The worker *clones*
its `surface_buf` (`concurrency.rs:100`) onto the notice at completion; the
channel's send/recv synchronises this *before* any later in-script
`replay_deferred_surface` drain (`concurrency.rs:333`, which `mem::take`s the buffer
on `await`, `concurrency.rs:538`), so the two never race. If the model joins
in-script, the cards replay live and the `joined` flag suppresses the notice; if it
never joins, the notice renders them at the boundary. Either way, once.

### The model-facing contract changes (as in the sibling)

The canonical idiom is no longer *poll across turns*. `spawn` returns immediately,
runs in the background, and the host **notifies** when it finishes. The system
prompt (`exarch/data/ral.md`) and the inline-timeout error
(`exarch/src/shell_eval.rs:165`) are rewritten: don't poll; don't `await` unless you
need the value in *this* same script. `await`/`poll`/`race` remain — the in-script
join and the on-demand pull — but they are no longer the *cross-turn observation*
mechanism. This **supersedes** the cross-turn poll idiom of
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]],
leaving the handle/detachment model intact. (Shared verbatim with the sibling; the
contract change is independent of which mechanism delivers the notice.)

## Safety — why the worker may hold `notify`

The sibling withholds *every* host channel from the detached worker, on the
standing invariant that detachment may hold only root/handle-owned resources, never
a foreground frame's channel ([[decisions/260617_watch-repl-builtin|watch-repl-builtin]],
[[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]]). That
withholding is correct **for the foreground `Emitter`**: a late presentation emit
splices a card into root's *sealed* scrollback — an ordering hazard. This design
respects exactly that hazard while reading the invariant precisely:

- **The hazard is a property of the delivery regime, not of holding a channel.**
  `notify` cannot splice: every value on it lands at the next turn boundary as a
  fresh turn, by construction — the same discipline that makes the inbox safe for
  the async `agent`'s post-turn `AgentResult`. So holding `notify` past the turn is
  not the withheld capability; holding the *foreground* channel is.
- **The worker holds only session- and root/handle-owned resources.** It holds its
  root-parented scope, its own buffers, and `notify` (session-owned) — the same
  class the async `agent` worker already holds (`tools/agent.rs` captures the
  inbox). It never holds the foreground turn's `Emitter`.
- **Safety is by construction, not discipline.** The sibling rightly rejected
  "give the worker the `Emitter`, trust it to call only `.inbox()`" — one stray
  `.emit()` re-creates the bug. `notify` has *no* foreground `.emit()` to misuse:
  its only destination is the boundary queue. The narrow capability is the type,
  not a rule a reviewer must enforce.
- **Live render stays foreclosed.** `notify` fires at *completion*, never
  mid-flight, so it never competes for the [[decisions/260619_terminal-lease|terminal-lease]].
  A still-running child raising a card into the live frame is still the open
  question of [[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]];
  this does not touch it.

## Why this shape

- **It honours the one-channel instinct at the right grain.** The author's "one
  channel" was right to refuse a pipe-per-purpose; it erred only in collapsing the
  *lifetime* axis too. "One channel per lifetime, each dispatch-by-class" is the
  same instinct, correctly scoped: two channels, ever — not one strained over two
  lifetimes, not three drifting toward N.
- **It is the next step on the same road, more cheaply.** `surface-carries-documents`
  opened the card set; `surface-reads-writes-execs` put core on the channel
  (*operation* vs *appearance*); the sibling separates *render* vs *control on one
  channel*. This separates *sync* vs *async into two channels* — and the async
  channel absorbs the "control" need *without* a non-rendering class, because a
  completion is just an event with a later lifetime, not a different kind of thing.
- **It deletes machinery instead of adding it.** No `` `spawn-started `` control
  class, no exarch waiter thread, no shell-free blocking settle, no two-observer
  cache handoff. The worker is already a thread holding its own result; it *emits*,
  it is not *observed*.
- **It keeps `events.json` truthful with no special-casing.** The async value is
  data (no live handle), so it serialises like any surfaced value. The sibling must
  route its control event *away* from the log because a handle cannot serialise;
  here there is nothing to route around.

## Alternatives considered

- **One channel; dispatch the post-turn need as an in-turn control class** — the
  sibling [[decisions/260622_surface-carries-control|surface-carries-control]].
  Correct and safe, but it pays for forcing a post-turn need through the in-turn
  pipe: a non-rendering control class, an exarch waiter thread, a new shell-free
  core settle, and an atomic two-observer cache handoff (its own stated sole risk).
  This proposal trades all of that for one extra sink of the shape we already have.
- **A pipe per purpose** (`detached_sink` now, a third tomorrow). Rejected: it makes
  the channel count track *purpose*. Lifetime is the only axis a tag cannot encode;
  every other purpose is a class, so only lifetime earns a new channel.
- **Hand the worker the foreground `Emitter`, disciplined to inbox-only.** Rejected
  for the sibling's reason and ours: it is the *presentation* channel, and "only
  call `.inbox()`" is discipline. `notify` is a distinct, narrow capability that
  cannot reach the foreground at all.
- **A worker-side completion callback in core (`Turn` gains a notify hook).** This
  *is* the present design, and the sibling's objection — "it teaches core a host
  notion (a sink that outlives the turn) and re-introduces a producer-held channel"
  — is answered, not dodged: core already holds a host-supplied `Arc<dyn EventSink>`
  for a worker past the turn today (the buffering `DeferredSurface`,
  `concurrency.rs:132`), so a second session-lived sink of the same trait is no new
  *kind* of thing; and a narrow `notify` defeats the discipline objection that
  actually applied to handing over the whole `Emitter`.
- **Auto-fire `notify` from the runtime vs a model-facing `notify` builtin.** For
  `spawn` completion the runtime fires it — the model writes nothing. Exposing
  `notify <value>` to the script for mid-life async updates is a *future use of the
  same channel*, deferred (see open questions), not needed now.

## Consequences

- Two host channels, split by lifetime, each dispatch-by-class. A future async
  effect is a new class on `notify`; a future in-turn effect is a new class on
  `surface`. Neither is a new channel.
- `spawn` notifies; the poll idiom and the poll-instructing inline-timeout text
  retire (shared with the sibling).
- The detachment invariant holds **by construction**: the worker holds `notify`
  (session-owned), never the foreground `Emitter`.
- `events.json` sees the async value as ordinary data; no special-casing and no
  serialisation problem, because nothing on `notify` carries a live handle.
- Core gains **one** `TurnRequest` field and one structural `spawn-done` value — no
  waiter thread, no shell-free settle, no two-observer handoff.
- Deliver-once is a boundary-time `joined`-flag check, not an atomic critical
  section: a lost race yields at worst a duplicate *notice*, never a lost or doubled
  *value*.

## Open questions

- **`joined`-flag semantics.** Exactly which observations claim the handle —
  `await`/`race` certainly; a *settled* `poll` (which hands the model the outcome as
  data) probably; a *pending* `poll` certainly not. Pinning this down is low-stakes:
  the failure mode is a redundant boundary notice, never a correctness fault.
- **`notify` as a model-facing builtin.** Whether the language should expose
  `notify <value>` for mid-life async updates (not just runtime completion) is
  deferred; it would be a use of this channel and runs straight into the live-render
  question below.
- **Live render from a running child.** Unchanged and still foreclosed
  ([[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]],
  [[decisions/260619_terminal-lease|terminal-lease]]): `notify` is
  completion-and-boundary only. A worker raising a card *mid-flight* still competes
  for the foreground and is not solved here.
- **`spawn` signature.** The notice reads better with a label, but — unlike the
  sibling, whose `` `spawn-started `` payload *needs* one — the mechanism does not
  require it (the worker can label the notice with the handle's `cmd`, which renders
  `<handle:…>`, `core/src/types/value.rs`). Whether to require `spawn "build" { … }`
  (mirroring `watch`, `concurrency.rs:225`) is a contract choice, not a mechanism
  constraint.

## See also

[[decisions/260622_surface-carries-control|surface-carries-control]] (the sibling
proposal this is set against — one channel + a control class vs two channels split
by lifetime),
[[decisions/260619_surface-carries-documents|surface-carries-documents]] and
[[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]] (the
render-document and core-on-channel classes — *operation* vs *appearance*, which
this extends to *sync* vs *async*),
[[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]] (the
inbox the async `agent` already posts to, and the background-surfacing open question
this partly answers),
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]
(the handle/detachment model and the poll idiom this refines),
[[decisions/260617_async-agent-tool|async-agent-tool]] (the inbox-posting precedent
the worker mirrors),
[[decisions/260617_turn-local-state|turn-local-state]] (`surface` is turn-local —
why `notify` is its session-lived sibling, not a mode of it),
[[decisions/260617_watch-repl-builtin|watch-repl-builtin]] (the
detachment-holds-only-root/handle/session-resources invariant),
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]]
(the surfaced value is a public `Value`, not core's repr),
[[map/exarch/shell-eval|shell-eval]] (the `AgentSink`/`value_to_card` seam `notify`
joins), [[map/exarch/cards|cards]] (the render subsystem),
[[map/exarch/tools|tools]] (the async `agent`/inbox path the worker mirrors),
[[map/core/builtins|map: builtins]] (`spawn` and the concurrency primitives), and
`docs/SPEC.md` §11.2 (the handle settle/replay rule, here left undisturbed — the
notice carries a copy, not the cache).
