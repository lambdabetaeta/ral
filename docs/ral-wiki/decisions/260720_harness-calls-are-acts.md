---
status: active
---

# Harness calls are acts, not observations

**A harness call changes the world outside the turn; a `ral` call reads it. The
two must not share a rendered shape.** Every `Kind::HarnessCall` today becomes a
`BlockKind::DiallableTool` — the same block a `ral` call mints — so a spawn, a
message, a schedule is an *observation*: it coalesces into the surrounding run of
reads and greps, wears a magnitude bar derived from nothing, and speaks in a
host-authored sentence that repeats its own verb. This decision gives acts their
own block kind, their own two rail shapes (`↗` fleet, `◷` time), and a
three-column row — **verb · subject · payload** — that carries the only
information an act ever has.

## The distinction

[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]] made the
transcript a graphic in Bertin's sense: *shape* tells kinds apart, *hue* names
the agent, *value* and *size* rank magnitude. Its Move 6 amendment then drew the
one structural line the rail obeys — **observation coalesces, a write is the
barrier**. Reads, greps and execs are recoverable context and compress hard; a
patch is the codebase's audit trail and can never be folded away.

A harness call sits on the *write* side of that line, and always has. `spawn`
starts a process that outlives the turn. `message` puts bytes in another agent's
inbox. `schedule` arms a wakeup that will fire tomorrow at nine whether or not
this session exists. `reply` ends the run. None of these is recoverable by
re-reading; none can be re-derived by asking again. They are the fleet's and the
clock's audit trail, exactly as a diff is the filesystem's.

So the governing principle:

> An **act** changes the world outside the turn. An **observation** does not.
> An act is a barrier: it renders standalone, in arrival order, and is never
> folded into a neighbouring run.

Everything below is a consequence.

## The diagnosis

`Block::observation()` (`tui/block.rs`) answers `true` for *every*
`DiallableTool`, with no discrimination by tool or verb, and `app.rs` routes
`Kind::ToolCall` and `Kind::HarnessCall` through one shared match arm that
dispatches purely on `summary.is_some()`. Four faults follow.

- **Acts coalesce.** A `message` landing between two `ral` calls is swallowed by
  the run scan and contributes an intent row and a sparkline bar to a block that
  is otherwise a burst of file reads. The barrier rule is violated by an object
  that is *more* consequential than the diff that honours it.
- **The magnitude is fake.** `Kind::HarnessResult` shares the `set_result_size`
  arm, and a desk result is always one line, so every act stamps `1` and draws
  the identical bar. Bertin's charge from the parent ADR applies verbatim: a
  variable that does not vary carries no information, and constant ink is
  decoration. Worse, `spawn` and `reply` emit no result at all, so an unrelated
  later `ral` result can attach *its* magnitude to their row.
- **The prose repeats the verb.** `agent  Agent hunter spawned (agent).` The
  host authors a full sentence at each emit site; the sentence's entire content
  is the verb, the subject, and — sometimes — the payload. Six emit sites each
  spell their own English.
- **One act renders two ways.** `schedule` passes the caller's label as the
  `summary`, so a labelled schedule is a dialable card and an unlabelled one is a
  bare inert `PlainTool` row. A label is a fact about the *subject*, not about
  the shape of the act.

The subject does not exist in the wire vocabulary at all: `Kind::HarnessCall`
carries `verb`, `cmd`, `summary`, and every agent name and schedule label is
baked inside the host sentence. Three columns cannot be rendered without putting
the subject on the bus.

## The decision

**Two new rail shapes.** `↗` for outbound fleet acts — `spawn`, `cancel`,
`message`, `reply` — and `◷` for time acts — `schedule`, `unschedule`. Shape is
Bertin's associative variable: it tells kinds apart, which is exactly what an act
needs and all it needs. `↗` is the outbound half of a pair whose inbound half
already exists: `↘`, the subagent landing, is **unchanged**. Both glyphs are one
display column, so `RAIL_W` stays 2 and the copy contract, wrap hang and
selection arithmetic are untouched. Both earn `RAIL_SHAPES` entries with real
legend copy — the table, not the enum, is what `is_rail_prefix` validates
against, so an omission there silently leaks the glyph into copied text.

**No size bar, no sparkline, no value step.** The act block reports
`magnitude() == None`, which leaves `rail::span` at the base agent hue and leaves
no bar derivable. `Kind::HarnessResult` stops feeding `set_result_size`. The
magnitude was never a quantity; deleting it is the Gantt-ribbon lesson applied a
second time.

**Three columns, the verb pinned.** Verb, subject, payload. The verb column is
pinned to a module constant sized by the longest verb (`unschedule`), so verbs
align down the page across separate blocks — `render_field_rows` cannot supply
this, because it derives its column width from the row set it is handed and an
act block is a single row. Verbs are the bare imperative. The subject is the bare
agent name or schedule label, unquoted. `reply` has no subject and leaves the
cell blank.

```text
↗ spawn      hunter   audit every unwrap() in exarch/src
↗ message    hunter   focus on the renderer first
↘ hunter 47s ███░░░░░  [failed: timeout]
↗ reply               [status: "clean", findings: 0]

◷ schedule   nightly  0 9 * * 1-5
◷ unschedule nightly

↗ cancel     hunter   refused: not a descendant
```

Content width 100, rail 2, so 98 usable: verb 11, subject 9, payload 78 — one
column of which the ellipsis reserves. The `↘` row above is the existing subagent
landing, shown only to place the pair; it keeps its bar because a subagent result
has a real magnitude to rank.

**The payload truncates; the dial keeps the rest.** `truncate_spans` already
appends a width-aware `…`. The full text stays at L3, and the act block joins the
dial the way `Subagent` does — dialable, floored at `Summary` (Census is
meaningless for a single act), rail shape level-invariant. Preserving dial-open
is the one thing the current design gets right.

**One act, one shape.** `schedule` renders as `◷ schedule` whether or not a label
was passed: label in the subject cell, blank if absent, the trigger description
in the payload. The act path never dispatches on `summary.is_some()`.

## Failure is a tier on the act row

An act that was refused is still an act *attempted*, and the row that names it is
the right and only place to say so — in the error tier, on the outcome, hot.
`cancel` is the instructive case: it has no argument payload at all, so its
payload column is necessarily its outcome (`refused: not a descendant`).

What this replaces is a screen-side duplication: the host-authored prose in
`HarnessCall.summary` *plus* `Kind::HarnessResult`, whose body the TUI never
prints and consumes only for the fake line count. Both go; the failure tier says
it once.

**There is no `╳` to remove.** An earlier reading of this design asserted that a
refused desk verb also draws an error chrome row. It does not. Every
`Kind::Error` emit site in exarch was enumerated, and none lies on the
agent-cancel / message / schedule / unschedule / reply refusal path. A refusal
*raises*; the raise leaves through `ExarchDesk::handle` as an `EnquiryError`,
fails the builtin body, aborts the ral program, and is written by
`report_runtime_error` as an ariadne report into the **captured stderr**, which
`digest::render` folds into the model-facing `STDERR:` / `EXIT: 1` sections. That
channel is wholly disjoint from the bus kinds the rail draws.

Two consequences, both load-bearing:

- **The model's copy is untouched, byte for byte.** Nothing in
  `HarnessCall.summary` ever reaches the model; nothing in a result body ever
  reaches the rail. Rewriting the summary or dropping `HarnessResult` is
  therefore *provably* model-invisible. The raises stay exactly as they are — the
  long-form sentence is for the model, and it is the model's only copy.
- **`HarnessResult` is not deleted from the bus.** It still rides `Kind` to the
  live display; `agent/transcript.rs` (since deleted,
  `dev/docs/plans/260814_kind_dissolves.md`) is no longer the forensic trace
  that kept it — `record.jsonl`'s own `Display`/`Forensic` commits are. Only
  the TUI's consumption of it — the fake line count — goes away.

The one genuine act that both emits `Kind::Error` and re-raises the same string
is an **OS thread-spawn failure** on `agent-start`. That is a resource failure,
not a refusal, and the `╳` is the only signal that the spawn died. It stays.

## Ruling: a refused spawn draws nothing

`agent-start` raises *before* `spawn_async` emits its `HarnessCall`. Every guard
— the stale fuel snapshot, exhausted spawn fuel, a name collision, no parked
fork, policy narrowing, fork/import/assemble failure, the door's decode checks —
therefore produces **no rail row at all**.

This is correct and must not change. A spawn that never happened is not an act:
nothing outside the turn moved, so there is nothing for the graphic to record.
Drawing a row would assert an event the world never saw, which is the same fault
as the fake magnitude in a different register. The model still receives the full
refusal through the raise. No row, no shape, no failure tier is added for it.

The same reasoning covers the other pre-chrome refusals: the self-wakeup grant
check, `reply` on a non-returning agent, and every payload decode. They are all
already screen-silent, and they stay that way.

## Why this shape

- **The barrier rule is applied where it was always meant to apply.** Once the
  act block answers `false` to `observation()`, the existing run scan ends the
  run at it and `reflow` renders it standalone — no edit to the scan itself. The
  structure was right; only the classification was wrong.
- **Shape carries kind, and nothing else has to.** `↗` and `◷` are two more
  entries in a vocabulary whose legend, copy-stripping and wrap-hang are all
  derived from one table. The cost of a new kind is three lines and a sentence of
  copy.
- **Structure beats prose.** Verb, subject and payload are the columns because
  they are the only fields an act *has*. A pinned verb column turns a page of
  acts into a scannable list; a sentence turns it into six sentences that must
  each be parsed to find the same three facts.
- **Hue still names the agent.** An act row carries no magnitude, so its rail
  sits at the unlightened agent hue — which is precisely the reading that "no
  quantity here" should have.

## Alternatives considered

- **Keep acts as tool calls and merely suppress their bar.** Rejected: it fixes
  the fake magnitude and leaves the coalescing, which is the deeper fault. An act
  folded into a run of greps is misfiled, not merely mis-decorated.
- **Give acts a magnitude from something real** (message length, prompt length).
  Rejected: an act's consequence is not proportional to its argument's size. A
  four-word `cancel` ends a subtree; a long prompt may spawn an agent that does
  nothing. Ranking would be a lie in the ordered channel, which the parent ADR
  reserves for quantities that are actually comparable.
- **One rail shape for all six verbs.** Rejected: fleet acts and time acts differ
  in *when* their effect lands — one now, outbound; one later, on a clock — and
  shape is free. Splitting also makes `↗` legible as the outbound twin of the
  existing `↘`.
- **Draw a row for a refused spawn.** Rejected as above: it would record a
  non-event.
- **Adopt the minted `sched-<n>` default label as `schedule`'s subject.**
  Rejected: it would put the registry's answer in a cell reserved for what the
  caller named, and the caller, by not labelling, declined to name one. An
  unlabelled schedule leaves the subject cell blank.

  This is the narrow rejection. An earlier draft rejected the wider claim —
  emitting `schedule`'s row *after* the registry call at all — and paid for it:
  a schedule the registry refuses (reserved label, duplicate label, no next
  occurrence) was emitted before the outcome existed, so `failed` was always
  `false` and the row read as one that landed. That made `schedule` the single
  act whose error tier was unreachable, breaking the rule above for one verb.
  The two questions are separable: the row is emitted after the registry call,
  so the outcome is known and can tier, *and* the subject stays the caller's own
  label. The trigger description is captured before the call, so a landed
  schedule's payload is unchanged.

## Amendment (2026-08-06): a required label dissolves the subject conflict

*Written once the desk's recording unified into one [`Observed::Act`] per
attempt (`verb`, `subject`, `payload`, `refused`), fanned to both the rail row
and the audit fragment off that single datum
([[map/exarch/shell-eval|shell-eval]]).* One `subject` field feeding two
readers exposed a tension this ADR's own "Adopt the minted `sched-<n>`
default" rejection above had only deferred: the rail wants the caller's own
label, the audit wants the label `schedule` actually resolved, and the two
disagree exactly when the caller supplies none and the registry mints a
`sched-<n>` default in its place. One `subject` field cannot hold two
different strings.

The repository owner's ruling: `schedule` must always require a label from
the model. With no minted default left to mint, the rail's label and the
audit's label are the same string by construction — the conflict is removed
at its source rather than arbitrated between two readers. The "narrow
rejection" above, and the unlabelled-schedule case it carved out, are both
moot: there is no longer an unlabelled schedule to reason about.
[`ScheduleRegistry::schedule`] drops the minting and the label parameter
becomes a plain `String`; [`crate::fleet::desk::payload_label`] refuses a
missing or malformed label at the wire before it ever reaches the registry.

## Out of scope

Headless (`exarch/src/headless.rs`) is untouched behaviourally. A stderr line has
no rail hue to carry identity and no column budget to align, so `[tool: {verb}]`
stays exactly as it is; the file changes only as far as the new `Kind` fields
force it to keep compiling, with its output byte-identical.

## See also

[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]] (the
Bertin encoding and the observation/barrier rule this extends),
[[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]] (the
effects that make a `ral` call an observation),
[[decisions/260706_enquiry-channel|enquiry-channel]] (the desk path a harness
call travels), [[decisions/260719_agent-names-and-schedule-labels|agent-names-and-
schedule-labels]] (the names and labels that fill the subject column), and
[[map/exarch/frontend|frontend]].
