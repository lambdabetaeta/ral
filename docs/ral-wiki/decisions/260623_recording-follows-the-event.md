---
status: accepted
---

# Recording follows the event, not its emitter

**What lands in a durable log must be decided by *what an event is*, not by *which
thread happened to emit it*.** A session owns two records — `events.jsonl`, the
append-only log whose fold is the model view, and `transcript.jsonl`, the
operational view (what the agent did) — and both are written *for every durable
session*, root and each forked child, in headless exactly as in the TUI.
`--no-logs` is the explicit transient exception. The operational
trace is fed at the one emit seam ([`Emitter::emit`](../../../exarch/src/bus.rs)),
so a fact is recorded because it *is* an operational event, regardless of who
raised it. The screen is a *consumer* of that stream — it never invents events;
the worker never narrates the screen.

This is the same discipline [[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]]
applied to the rail and [[decisions/260619_surface-carries-documents|surface-carries-documents]]
applied to kit content, now applied to the **event vocabulary and where it is
recorded**: name the *role* of a thing, not its appearance, and record it once at
the point it is announced.

All of this is **implemented**.

## Superseding note: the model record is the log

The context design [[decisions/260812_context-is-a-projection|context-is-a-projection]]
supersedes the old shorthand "`events.jsonl` is the model view". `events.jsonl`
is now the durable event log whose left fold determines the model view. Its
membership test is therefore: does the event determine the projection, or is it
a forensic breadcrumb? Both are legitimate log records. `ContextEdited` is the
new example: its `Fold`/`Drop` operation and authority determine future context,
while a `Nudge` may be retained as evidence without becoming a model message.
The old `Compacted` record exposed the failure mode — it said that a compaction
happened but did not record its cut, so replay could not reconstruct the view.

The two durable records remain separate. Resume pays their rent through coupled
rotation, a shared wall-clock stamp as the cross-file join key,
`Transcript::rotate` swapping a writer inside a shared `Arc<Inner>`, and
`ContextEdited` being recorded in both. The debt is named **two-record
unification**. Events that project to no messages now provide the dissolving
mechanism: the forensic record could become a filtered projection of the one
log. What blocks deletion is the bus — emitters are cloned across threads,
while `AgentLog` appends under one mutex on the attend thread. Keeping the split
costs coupled rotation and its half-failure policy, timestamp/join machinery,
shared writer indirection, duplicate edit emission, and two durable readers
whose ordering must still be related. The next design should price those costs
against deleting a record, not add a third trace by habit.

## The diagnosis

The bus began life as a *display* stream with exactly one consumer — the frontend
that draws it. Under one consumer, "on the bus" and "shown to the user" were the
same fact, and three shortcuts were harmless that are not harmless now.

- **Recording-worthiness was decided by origin.** The worker can only speak
  through `Emitter::emit`, and emit now also writes the trace — so *everything*
  the worker says is recorded, even a no-op acknowledgement. The TUI, conversely,
  draws by synthesising an `Event` and feeding its own `handle`
  (`exarch/src/tui.rs:3088`, `:3096`) — bypassing the emit seam — so *nothing* the
  UI says is recorded, even a real state change. The line between "logged" and
  "not logged" fell where the threads happened to sit, not where meaning is. Two
  concrete wrongs result: a **model switch** (`tui.rs:3088`) is operational but
  the UI raises it, so it is lost from the trace; **"nothing to compact"**
  (`exarch/src/session.rs:813`) is a no-op but the worker raises it, so it is
  recorded.

- **The screen invents events.** `apply_model_switch` (`tui.rs:3057`), the slash
  legend (`tui.rs:2840`), the clipboard ack (`tui.rs:2876`), the export ack
  (`tui.rs:2906`), and `note_error` (`tui.rs:3096`) all build `Event { kind: … }`
  values and push them through `App::handle`. Some of these are view decoration
  that should never be recorded (legend, clipboard, export); one is a genuine
  event that should (model switch). The same mechanism serving both is why neither
  outcome is correct.

- **The vocabulary names appearance, not role.** `Kind::Dim` names *how a thing is
  drawn* (dimly), where every other operational `Kind` — `Step`, `ToolCall`,
  `Usage`, `StopReason`, `Nudge`, `Error` — names *what happened*. The cost was
  invisible while the bus only fed a renderer; the moment the trace became a
  second consumer, the operational log began carrying `{"kind":"dim"}` — an
  appearance word in a record meant to say what the agent did. A post-mortem
  reader learns the colour, not the meaning.

- **Operational notes leak into the model view.** `note_dim` (`session.rs:922`)
  writes *both* `events.jsonl` (via `record_dim`, `exarch/src/event.rs:704` →
  `SessionEvent::Dim`, `:227`) and the bus. But a compaction notice or a model
  switch is not a message the model saw; it has no place in the artefact whose job
  is exact model-context replay.

## The decision

### A per-session operational trace, fed at the emit seam

`transcript.jsonl` is owned by the [`Session`](../../../exarch/src/session.rs),
created in `Session::assemble` beside its `events.jsonl`, so the root and every
forked child get one — in headless and the TUI alike. It is a
[`Transcript`](../../../exarch/src/transcript.rs) handle the
[`Emitter`](../../../exarch/src/bus.rs) carries; `Emitter::emit` tees every event
to it before broadcasting. Recording therefore rides the emitter, *not* the bus
channel's lifetime: a headless child muted from the display (its receiver dropped,
`exarch/src/tools/agent.rs`) still records its full trace, because `session_lived`
now governs only *streaming to a live tab*, not *persistence*. The trace records
operational events only — the rendering-only `Card`, the card half of a
`Kind::Io`, and `Kind::Phase` project to nothing (`exarch/src/transcript.rs`
`event_record`); their place is the rendered `user.log`.

### Nudges are operational

A driver nudge is the harness re-prompting the model; it is something that
happened, not a thing drawn. `Kind::Nudge { used, max, cause }`
(`exarch/src/bus.rs`) is recorded in the trace at the emit seam; the display
chooses whether to surface it (a `[nudge n/m: cause]` line in headless, quiet on
the TUI rail). Its `events.jsonl` twin (`SessionEvent::Nudge`) stays a forensic
breadcrumb in the log. This proved out the principle: "off the display" and "off the
trace" are now distinct, decided per consumer.

### Name the role, not the appearance: `Kind::Dim` → `Kind::SystemNote`

`Kind::Dim` becomes `Kind::SystemNote(String)`: an operational note the agent's
driver issued — a truncation recovery, a compaction step. The display still
renders it dim; *dimness is the renderer's choice*, not a fact in the vocabulary.
The trace records `{"kind":"system_note"}`, which says what it is.

### No operational notes in the model view

`SessionEvent::Dim` and `record_dim` (`event.rs`) are removed. `note_dim`
(`session.rs:922`) becomes `note`, which emits `Kind::SystemNote` through the
emitter — recorded in the **trace only**. Operational notes are uniformly
transcript-only; the model projection is determined by the event log, which may
also retain forensic breadcrumbs. This is the
model/operational split made total: the same kind of fact lives in one place
whoever raised it.

### The screen never invents events; the worker never narrates the screen

The UI stops synthesising `Event`s. It does exactly two honest things — draw the
events that arrive, and draw its own local decoration — and each former
`App::handle(Event{…})` injection sorts into one of them:

- **Model switch** is a real event. `apply_model_switch` (`tui.rs:3057`) is given a
  recording emitter (minted on the UI thread from the bus, carrying the root's
  `transcript()`, cloned before the worker takes the session) and **emits**
  `Kind::SystemNote("[switched to …]")`. It records in the trace and draws through
  the normal path — no UI-invented event, no special save.
- **View chrome and UI errors** — the slash legend, clipboard ack, export ack, and
  `note_error` — are view decoration, never recorded. They draw straight to the
  viewport through a small helper (`App::push_note` / `App::push_error`) instead of
  building a counterfeit `Event`. Drawn, not recorded — which is correct, and now
  they no longer masquerade as events.
- **"Nothing to compact"** (`session.rs:813`) is dropped. It is a no-op
  acknowledgement, not an event; the absence of a `compacted` note already says
  nothing happened, and the worker has no honest way to draw view-only chrome.

## Why this shape

- **One axis, decided on purpose.** "Is this recorded?" becomes a property of the
  event's kind, evaluated once at the emit seam, instead of an accident of which
  thread holds which handle. The rule is legible: operational kinds record; view
  decoration does not; the screen draws both but authors neither.
- **The vocabulary stops lying.** A `Kind` answers *what happened*; the renderer
  answers *how it looks*. `SystemNote` + a dim rendering restores that split, the
  same one [[decisions/260619_surface-carries-documents|surface-carries-documents]]
  draws between a mark's *level of measurement* and its visual variable.
- **Two records, one job each.** `events.jsonl` is the event log a replay or the
  protocol state machine folds into the model view; `transcript.jsonl` is the
  operational view a post-mortem reads. Pulling ordinary notes out of the log
  keeps the split honest, while projection-neutral breadcrumbs remain allowed.
- **Recording survives muting.** Because the trace rides the emitter and not the
  channel, every forked child records its own trace whether or not anyone watches
  it — the property "all children are recorded" holds by construction.

## Alternatives considered

- **Give the UI a way to save (the first instinct).** Rejected: it makes the
  screen a second author of durable state, the exact conflation this ADR removes.
  The UI should *cause* events, not save them.
- **Keep `Kind::Dim`; let the trace filter it out.** Rejected: it leaves an
  appearance word in the operational vocabulary and pushes the role/appearance
  confusion downstream into every consumer.
- **Keep operational notes in `events.jsonl` too.** Rejected: it would make
  ordinary operational notes part of the event vocabulary without making them
  determine the projection. Forensic breadcrumbs are a different category and
  are retained by the superseding context decision.
- **A second emit path, "broadcast without recording," for worker-side view
  chrome** (to keep "nothing to compact" on screen). Rejected as YAGNI for a
  single no-op message; dropping it is the honest move, and the only genuine
  worker view-chrome case does not exist once that one is gone.
- **Record the view chrome too (legend, clipboard, export).** Rejected: they are
  decoration of the view, not facts about the run; recording them would re-import
  the noise the model/operational split exists to keep out.

## Consequences

- A model switch now appears in the trace; "nothing to compact" no longer does —
  recording finally tracks meaning.
- `Kind::Dim`, `SessionEvent::Dim`, `record_dim`, and the four UI `Event`
  injections retire; the UI gains `push_note`/`push_error` and a recording emitter.
  The bus vocabulary shrinks by an appearance word and gains a role word.
- The trace is uniform: every operational note is `system_note`, transcript-only,
  whoever raised it; `events.jsonl` holds projection-determining turns and its
  forensic breadcrumbs (`Nudge`, `ProviderError`).
- The screen is a pure consumer again: it draws events and its own chrome, and can
  no longer fabricate durable state.

## Implementation plan

Landed in order; each compiled and the suite passed between steps.

```
1  Rename       Kind::Dim -> Kind::SystemNote; render dim in tui + headless; trace projects "system_note"
2  Model view   delete SessionEvent::Dim + record_dim; note_dim -> note (emit only); update event.rs tests
3  UI causes     UI-thread recording emitter; apply_model_switch emits SystemNote; drop the app.handle inject
4  UI chrome     App::push_note / push_error; legend, clipboard, export, note_error draw direct (no Event)
5  Drop          remove the "nothing to compact" note entirely
```

No new tests; the `event.rs` `Dim` round-trip test was rewritten to assert only
the model-view breadcrumbs (`Error`, `Nudge`) that remain. Threading the
UI-thread emitter through the command path bundled the picker handles into a
`CommandCtx` (`exarch/src/tui.rs`), so `ui_loop` → `route_submit` → `pick_model`
→ `apply_model_switch` carry one context rather than a fistful of handles.

## See also

[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]] (name the
datum's role, never its appearance — the principle this extends from the rail to
the event vocabulary), [[decisions/260619_surface-carries-documents|surface-carries-documents]]
(the `Card`/`Mark` split between data and presentation; `events.jsonl` as a
structured machine record), [[decisions/260618_run-turn-host-loop|run-turn-host-loop]]
(core carries raw `Value` and names no rail vocabulary; this is an exarch-only
concern), and the as-built arm it re-grounds: [[map/exarch/frontend|frontend]],
[[map/exarch|map: exarch]].
