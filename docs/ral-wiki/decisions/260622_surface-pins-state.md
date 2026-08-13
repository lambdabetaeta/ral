---
status: accepted
---

# A rendered value is an *event* or *state*: `surface` pins state to a register, in place

**The transcript is an append-only log of *events* — things that happened, in
order, sealed once written. But a kit that maintains a task list, a build's
status, or a watch's latest value is not narrating events; it is publishing
*state* — what is currently true — and state changes by being *overwritten*, not
appended. exarch already renders model-independent state this way (the agent
matrix, the `ctx%` gauge, the phase bar: fixed-position marks redrawn in place,
never streamed, never logged). It has no place for a *kit* to do the same. So
`tasks.ral` does the only thing it can — it `surface`s a fresh `` `card `` on
every `transition`, marching `tasks 0/3`, `tasks 1/3`, `tasks 2/3` down the
scrollback — which is exactly the streaming the rail doctrine forbids
([[decisions/260618_tui-transcript-as-graphic|transcript-as-graphic]]).** The
fix is to give `surface` a second *disposition* for its render class: a value may
**append** (an event, today's scrollback block) or **pin** (state, a keyed slot
in a persistent *register*, overwritten on re-surface). The register is the
**model-authored dual of the matrix** — same graphic grammar, opposite author.

This is the mechanism that turns `tasks.ral` from the rail doctrine's
counterexample into its first client: the changing datum lives at a fixed
position and is updated in place, because the kit can finally *say so*.

## Context

### The surface lineage, and the cut this ADR adds

`surface <value>` (`core/src/builtins/misc.rs`) forwards one `Value` to the
turn-local `EventSink` ([[decisions/260617_turn-local-state|turn-local-state]]);
exarch's `AgentSink::emit` (`exarch/src/shell_eval.rs:71`) decides what each value
*is*. The channel's meaning has been recharacterised twice already:

- `surface-carries-documents` opened the render-document class — a `` `card ``
  `Variant` of Bertin marks, decoded by `value_to_card`
  ([[decisions/260619_surface-carries-documents|surface-carries-documents]]);
- `surface-reads-writes-execs` put **core** on the channel and separated
  *operation* from *appearance*
  ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]);
- `surface-carries-control` separated *render* (`` `card ``, `io`) from *control*
  (`` `spawn-started ``), and named the channel for what it is — the single typed
  `Value` channel from the language to the host, dispatched **by class**
  ([[decisions/260622_surface-carries-control|surface-carries-control]]).

Each step refined the taxonomy by asking *what the host does with the value*.
This step asks the question one level finer, *inside* the render class: a render
is not always an **event**. `surface-carries-control`'s own table reads "render
classes reach the rail **and the log**" — append-only scrollback, an
`events.jsonl` row. That is the right home for *a thing that happened*. It is the
wrong home for *what is currently true*.

### The thing the taxonomy is missing: state

Everything exarch puts on screen today is one of three:

| kind | example | where it lives | overwritten? | in `events.jsonl`? |
|---|---|---|---|---|
| **event** | a tool call, prose, a diff, an `io` effect | transcript (scrollback) | no — append-only | yes |
| **host-authored state** | the agent matrix, `ctx%`, the phase bar | pinned chrome | yes — redrawn each frame | no |
| **control** | `` `spawn-started `` | nowhere (host effect) | — | no |

The matrix (`matrix_bar`, `exarch/src/tui.rs:2078`) is the proof exarch can hold
evolving entities at fixed positions and redraw them in place: one row per agent,
magnitude lightened relative to the frame's heaviest spender, no streaming, no log
row. That is the doctrine realised — for state the *host* owns.

There is a fourth cell, empty: **model-authored state**. A kit holds a datum that
changes over a session — a task rollup, a long job's status, the file currently
under edit — and wants it shown the way the matrix shows cost: a fixed mark,
updated in place. The transcript cannot host it. Scrollback is append-only *by
construction* — `surface-carries-control` bends over backwards precisely to never
splice a late card into "root's sealed scrollback." So a kit with evolving state
has only two moves today: **stream it** (append a new card per change — what
`tasks.ral` does, and the doctrine's counterexample) or **don't show it**. The
register is the missing fourth cell, and it is the matrix's dual: same grammar
(rows of marks at fixed positions encoding level-of-measurement as visual
variables), authored by the kit instead of the host.

## Decision

### `surface` render gains a *disposition*: append (event) or pin (state)

The class dispatch of `surface-carries-control` is unchanged — render vs control.
This ADR splits the **render** class along a second, orthogonal axis, the
**disposition**:

| class | disposition | example | host action | overwritten? | in `events.jsonl`? |
|---|---|---|---|---|---|
| render | **append** | `` `card ``, `io` | `value_to_card` → scrollback block | no | yes |
| render | **pin** | `` `pin [key, body] `` | `value_to_card body` → register slot `key` | **yes** | **no** |
| control | — | `` `spawn-started `` | host effect, no render | — | no |

An *append* render is an event: it lands as a block, in order, and is logged. A
*pin* render is state: it writes a **keyed slot** in a session-lived register;
re-surfacing the same key **overwrites in place**. The key is model-chosen, and
*is the identity of the datum* — the thing the host cannot guess and the doctrine
needs named. Control is untouched; an unknown class or disposition degrades
gracefully (drop / plain ink), exactly as today.

### The wire format: a disposition wrapper, not a richer card

A pinned value is a `Variant` wrapping the existing render document:

```
`pin   [key: "tasks", body: `card [ `measure [label: "tasks", value: 3, max: 8] ]]
`unpin [key: "tasks"]
```

`body` is an ordinary `` `card `` (`Card(pub Vec<Mark>)`, `exarch/src/card.rs:122`),
decoded by the **unchanged** `value_to_card` (`card.rs:541`) — exactly as
`surface-carries-control` decodes a spawned body's buffered surfaces "as the live
surface would." The wrapper carries only *placement* (the key); the card
vocabulary, its marks, and its decoder are all reused verbatim. `` `unpin ``
drops a slot (a finished plan clears its gauge); an absent body is the same as
`` `unpin ``. This is deliberately *not* a new field on `` `card ``: threading an
optional disposition through every card would touch `value_to_card` and every
card builder, where a wrapper touches only the new arm.

### The host machinery: a register on the viewport, one new `emit` arm

`AgentSink::emit` (`shell_eval.rs:71`) gains a pin arm, tried before the existing
render arms:

```rust
if let Some((key, body)) = value_to_pin(ev) {        // `pin / `unpin
    match body { Some(card) => Kind::Pin { key, card }, None => Kind::Unpin { key } }
} else if let Some(event) = value_to_io(ev) { … }     // unchanged
  else if let Some(card)  = value_to_card(ev) { … }   // unchanged
```

The register lives on the **`Viewport`** (a sibling of `blocks`,
`exarch/src/tui/viewport.rs`): an *ordered* `key → Card` map, so pins render in a
stable insertion order. `App::handle` routes `Kind::Pin { key, card }` to
`vp.set_pin(key, card)` (overwrite or insert) and `Kind::Unpin { key }` to
`vp.drop_pin(key)` — the in-place analogue of `push_card`. `Viewport::reset`
(`viewport.rs:273`), which already clears `blocks` on `/clear`, also clears the
register, so a pin is generation-bounded by the same discipline as everything else.

The register renders as a **reserved right-hand column** for the *focused*
session — a flat rect glued to the right edge of the content area, **not** a
floating overlay. The distinction is the one the `/model` picker already draws —
*"flat in rendering — a strip, not a floating overlay"*: the column is a region
the layout reserves and the transcript sits left of, never a layer that occludes
— and so defeats — the selectable, yank-able scrollback. It is the picker's
principle rotated onto the *X*-axis.

**The right edge is freed by deleting the scrollbar.** The 1-column
`ScrollbarOrientation::VerticalRight` widget owned the right edge and would have
competed with the register for it; we retire it. Its datum — *where the window
sits in the scrollback* — is not lost but **re-encoded as a fixed-position
magnitude in the rule line** (`⇣ 72%`, `⇣ bot` at the tail), which is the
encode-don't-stream doctrine applied to the scroll position itself: a value mark
at a fixed spot, not a sliding glyph. So the deletion is *more* on-doctrine than
the bar it removes, and it makes the frame's left/right symmetry exact — the
**rail owns the left edge** (the data column, what happened), the **register owns
the right edge** (the state column, what is).

The placement is nearly free. `reflow` already wraps the transcript at
`width.min(READ_W)` with `READ_W = 100`, so the body only ever paints its leftmost
100 columns and any width past that is **dead space today**. The body row is split
by hand (not a constraint solver) into three regions: the rail's `LEFT_MARGIN`, a
transcript **capped at `READ_W`** so it never widens past the reading measure, and
the register glued to the right edge — the surplus between them is a flexible
reading gap. Pinning the transcript at the cap is what makes "the transcript does
not narrow at all" literally true: inserting the column steals only from the dead
margin, never from prose.

```
[LEFT_MARGIN][ transcript, capped at READ_W ][ flexible gap ][ register ]
    rail            the event stream            grows w/ width   the post-its
```

Each pin renders as a bordered `Card` — the "bounded object" framing surfaced
cards already wear — stacked top-down with a blank-row gutter, in the producing
agent's hue (the rail's `hue = agent` variable). Clicks hit-test through
`FrameGeom` exactly as scrollback blocks do, so the disclosure discipline holds:
the column shows the *ambient* datum (a `tasks 3/8` gauge, a one-line status), and
the full datum (the entire task list) is reached by **disclosure**, never by
permanently occupying the column. Two degradations, each following an established
pattern:

- **Narrow terminals** — below ~`LEFT_MARGIN + READ_W + gap + column` there is no
  free margin to claim without narrowing prose. The register **collapses** to a
  one-row **pin band** — each pin reduced to a one-line digest, laid out
  horizontally in a reserved strip beside the matrix — rather than narrowing the
  transcript. The same conditional-chrome pattern as the matrix appearing only
  with more than one session; the column is, by design, a wide-terminal luxury,
  and every narrower terminal still gets ambient feedback in the band.
- **Vertical overflow** — a column has the full frame height (far more headroom
  than a band would), but a kit that pins past what fits still needs a cap +
  oldest-key-drop or an internal scroll. The policy is named in open questions.

### The kit becomes a client; `tasks.ral` stops streaming

`tasks.ral`'s `add-task` / `transition` (`kit/tasks.ral:99`, `191`) stop
`surface`ing a `task-card` per change. Instead they pin one rollup:

```
surface `pin [key: "tasks", body: !{meter-card $done $total "tasks"}]
```

overwritten on every transition — one gauge that *fills in place* rather than a
column of `tasks N/total` blocks. A task moving `open → done` is a state
transition of a state variable, so it updates the gauge and appends nothing. The
genuine *events* — a `spawn`/`agent` that actually finished work — already have
their own transcript notices (`Turn::Spawn`/`Turn::Agent`,
[[decisions/260622_surface-carries-control|surface-carries-control]]). The
watchdog and the pure data layer are untouched.

## Safety — why this adds no lifetime hazard

`surface-carries-control` needed a careful concurrency section because a `spawn`
notification must cross the turn boundary, and the detached worker may hold no
host channel. **A pin needs none of that.** It is a *render-class* disposition,
emitted **in-turn** through the live foreground `AgentSink` — the exact place a
`` `card `` is already safe. There is no waiter, no detached channel, no
cross-turn delivery, no `delivered` latch. The only new mutable state is the
register on the `Viewport`, written in-turn and wiped by `reset`.

Two compositions to state explicitly:

- **A pin inside a spawned body** rides `surface-carries-control`'s deliver-once
  buffer: it is among the `surfaced` values on `SpawnResult`, decoded at the
  boundary "exactly as the live surface would," so it sets its register slot when
  the spawn's surfaces replay. State set slightly late is benign — it is the
  latest binding, not a missed event — so lateness costs nothing the way a
  dropped event would.
- **A pin from a *live* background child** is the *same* foreclosed case as a live
  card ([[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]],
  [[decisions/260619_terminal-lease|terminal-lease]]): a worker raising render
  mid-flight competes for the foreground. This ADR opens no new door there; it
  rides the same ones render already uses.

A pin is also *not* an inbox turn. `Turn` (`bus.rs:216`) is Human / Wakeup / Agent
(/ Spawn) — deliveries that become *new turns*. A pin delivers no turn; it mutates
ambient state within the current one. Keeping pins off the inbox is what keeps
"state" and "event" from blurring at the seam.

## Why this shape

- **It is the next step on the road already taken.** documents → (operation vs
  appearance) → (render vs control) → **(event vs state)**. Each step refined the
  surface taxonomy by one honest distinction; this is the next one, and the first
  that looks *inside* the render class.
- **It is the dual of a thing that already exists.** The register introduces no
  new visual vocabulary — it lets a *kit* author what the *host* already authors
  in the matrix. The proof-of-concept that pinned, in-place, unlogged marks are
  right is already on screen.
- **Position carries the distinction.** Rendering state in a reserved right
  column makes the frame's *horizontal* axis — Bertin's strongest variable — the
  event/state partition: the left rail and body are *what happened* (the event
  stream), the right margin is *what is* (the state register). The cut stops being
  implicit in which list a card lands on and becomes legible in *where on the
  plane* the mark sits, symmetric with the left rail already being the established
  data column. A flat reserved column, not a float, is what keeps that partition
  honest — an overlay would occlude the very scrollback it is meant to sit beside.
- **It makes the doctrine expressible.** "Encode the changing datum as a
  fixed-position magnitude, never streamed" is, today, enforceable only on
  host-authored marks; a kit had no way to obey it. Pin extends the doctrine to
  kit-authored state and converts `tasks.ral` from violator to exemplar.
- **It keeps `events.jsonl` truthful.** State is not an event. A `tasks 1/3 →
  2/3` overwrite is not "a thing that happened" worth a log row; the *completions*
  that are land as events through the existing notify path. The log keeps
  recording what *happened*; the register shows what *is* — the matrix's logging
  story exactly, one author over.
- **It is cheap and safe.** One `emit` arm, one viewport field, one draw band,
  one wrapper decoder. It reuses `value_to_card`, the deliver-once buffer, and the
  `reset`/generation discipline, and introduces no new lifetime invariant.

## Alternatives considered

- **A single status *line* instead of a keyed register.** Rejected: one slot
  cannot hold concurrent pins (a build status *and* a task rollup *and* a watch
  value), and it special-cases "the status line" rather than giving a primitive.
  The matrix already demonstrates that multiple in-place rows are wanted; the
  register generalises, the status line does not.
- **Keep kits streaming; have the host coalesce same-shape cards in place**
  (auto-pin by structural identity). Rejected: identity is the *kit's* to name,
  not the host's to infer. Structural coalescing is brittle — the `io_buf` /
  `patch_buf` grouping (`tui.rs`) shows how much care host-side coalescing already
  needs — and it would silently change scrollback semantics under kits that meant
  to append.
- **Fold tasks into the matrix as extra rows** (a `control` event the host renders
  into `matrix_bar`). Rejected: the matrix is *host-authored process* state
  (sessions, cost, steps) with the session lifetime; tasks are *model-authored
  intent* with the kit's lifetime. Conflating them overloads one projection with
  two ownership models and two lifetimes. They are duals — same grammar — not the
  same table.
- **A `key` field on the `` `card `` variant** rather than a `` `pin `` wrapper.
  Rejected (mild): it changes `value_to_card`'s shape and forces every card to
  carry an optional disposition; the wrapper keeps the card vocabulary and decoder
  untouched and composes by decode-the-body, the pattern `surface-carries-control`
  already uses for spawned-body surfaces.
- **Persist the register to `events.jsonl` as state snapshots.** Deferred (see
  open questions): the v1 register is ephemeral, the matrix's dual on the logging
  axis too.

## Consequences

- `surface`'s render class is documented as carrying a **disposition**: append
  (event, scrollback, logged) or pin (state, register, in place, unlogged).
  Adding a future pinnable datum is a kit `` `pin `` call, no host change.
- exarch grows a `Viewport` register, one `emit` arm, a reserved right-hand
  register column, and a bounded-with-disclosure rendering discipline. No new
  concurrency invariant.
- `tasks.ral` stops streaming: `transition` pins one rollup gauge, overwritten in
  place; per-task changes append nothing.
- `events.jsonl` and the transcript still record only *events*; pinned state is
  ambient, like the matrix, and is not logged as a row.
- The register is generation-bounded by `Viewport::reset` / `/clear`, exactly as
  scrollback is.

## Open questions

- **Snapshot persistence for replay.** The v1 register is ephemeral — it dies with
  the session, like the matrix, and a transcript replay restores only the events.
  If replay should restore the *final* register, it wants a distinct `state`
  snapshot record (not an event row), so the log stays an event log. Lean
  ephemeral; revisit if replay fidelity demands it.
- **Per-session vs cross-session pins.** Pins live on the focused viewport, so a
  subagent's pins show only on its tab. Whether root should see a child's pinned
  state is probably the *matrix's* job (it is the cross-session projection); left
  per-session for now.
- **Eviction when a kit over-pins.** The column is capped; what happens when pins
  exceed it — oldest-key drop, LRU, an internal scroll, or the kit's
  responsibility? The queued-strip cap is the precedent (cap + don't overproduce);
  the exact policy is unstated.
- **Disclosure mechanism.** Reuse the rail-dial idiom on a pinned mark, or a
  dedicated key? An implementation detail, but the *principle* (detail is
  disclosed, never resident) is load-bearing and decided here.
- **Horizontal budget and the matrix.** *Resolved.* The register column claims the
  right margin; the matrix claims rows; they sit on different axes. The
  scrollbar-adjacency conflict is gone — the scrollbar is deleted and the register
  owns the right edge outright. The width is `REGISTER_W` (a framed gauge plus
  borders) and the collapse threshold is `LEFT_MARGIN + READ_W + gap + REGISTER_W`;
  both are tunable constants in `tui.rs`. Below the threshold the register shows as
  the pin band. *Still open:* vertical overflow when a kit pins past a screenful —
  v1 clips at the column's height; a cap + oldest-key-drop or internal scroll is
  deferred.

## See also

[[decisions/260622_surface-carries-control|surface-carries-control]] (the
dispatch-by-class this refines — it split render from control; this splits render
into event vs state; note that ADR is still `proposed` and this depends only on
its stable kernel: surface is the typed channel, render decodes via
`value_to_card`, spawned-body surfaces are decoded "as the live surface would"),
[[decisions/260619_surface-carries-documents|surface-carries-documents]] (the
`` `card `` vocabulary the `body` reuses verbatim),
[[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]
(*operation* vs *appearance* — the previous render refinement),
[[decisions/260618_tui-transcript-as-graphic|transcript-as-graphic]] (the rail
substrate and the matrix this is the model-authored dual of, and the
encode-don't-stream doctrine this finally lets a kit obey),
[[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]] (the
bus/inbox split — a pin is deliberately *not* an inbox turn),
[[decisions/260617_turn-local-state|turn-local-state]] (`surface` is turn-local —
why a pin is emitted in-turn and needs no waiter),
[[decisions/260619_terminal-lease|terminal-lease]] (live background render stays
foreclosed; pins do not change that),
[[map/exarch/cards|cards]] (the render subsystem the `body` decodes through),
[[map/exarch/frontend|frontend]] (the draw layout the register band joins), and
`kit/tasks.ral` (the first client — the streaming counterexample this retires).
