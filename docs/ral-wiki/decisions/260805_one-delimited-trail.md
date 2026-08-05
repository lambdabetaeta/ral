---
status: active
---

# One delimited trail: a dispatch answers with an ending and its effects

**An unwind discards a call's bindings and keeps its effects, so what a
dispatch owes its caller is a pair — the ending, and the prefix of effects the
extent committed before it: `dispatch : Program → Ending × Trace`. Core
already reified the trace half (`Observation` as the one fact vocabulary,
`observe_stamped` as the one emission door, `Audit` as the collector,
`audit { }` as the in-language reflection), and the harness had been
rebuilding it per stratum — an act ledger, an epoch clock, a boolean smeared
off a sum, a stderr assembled by append order across two layers. This ADR
retires the copies. One mechanism — `delimit : Policy × Extent → Ending ×
Trace` under five laws — with four delimiters over the same collector
(`audit { }`, `try`, `ral --audit`, the exarch tool call), two author-only
contributors (the desk, host-side; the cross-process helpers, by fragment),
one total projection to every seam, and one renderer per audience. The ending
becomes a literal sum, `Ending`, stratified as two types along the
pre-existing `RunReport`/`Report` seam with `render_ending` the one lossy
projection between them; one `PROTOCOL_VERSION` bump (5 → 6) covers the
batch. A rider decided during implementation makes `schedule`'s label
mandatory.**

## Context

### The law, and the debt

A cancel of any cause unwinds as a sticky `Break::Error`, re-raised at every
poll point; the observations already pushed live in `shell.local.audit` and
are untouched by the unwind — `finish_command` records even the command the
cancel struck, with its error. A partial trail survives the unwind *by
construction*. Every sentence the harness owes the model at a raise — the
audit of what stands, the workers that outlived the wall, the per-stage
journal this wiki once called "deliberately absent" — is a projection of the
pair `(Ending, Trace)`, and core held all the machinery: `Observation`
(`core/src/types/observation.rs`), `observe_stamped`
(`core/src/evaluator/audit.rs`), the `Audit` collector
(`core/src/types/audit.rs`), and `audit { }`'s never-failing
`(status, value, error, children)` reflection.

The seam never used it. Four bespoke copies stood in its place:

| bespoke copy | what it approximated | what it is now |
|---|---|---|
| `ActLedger` / `struct Act` (`exarch/src/fleet/desk.rs`) | the trace's committed harness acts | `ActFragment`: `Observation`s in the shared vocabulary, minted at `HostServices::commit_act`; `DeskAct` survives as the verb-and-past-tense vocabulary |
| `birth_epoch` / `` `born-this-epoch` `` | "spawned inside this dispatch's extent" | `Observed::Worker` in the dispatch's own trail — membership is presence-in-trace, not clock arithmetic |
| `timed_out: bool` on `ToolResult` | the ending's identity | the `Ending::Walled` arm of the ending sum |
| stderr assembled across two layers (`shell_eval.rs` + `agent/shell.rs`) | one rendering of `(Ending, Trace)` | `shell_eval::report::render`, where order is code |

The tell was in our own docs, twice: [[design/audit|audit]] confessed "core
keeps no per-stage journal", and `evaluator/audit.rs`'s header listed
"exarch's host doors have no trail" as an exception to the one door — the
exception the `ActLedger` was born in. Even the words converged: the harness
printed a stderr line beginning `audit:` while the delimiter was named
`audit { }`.

The old lifecycle also leaked: `force_open` opened and nothing ever closed,
so after the first `try` or `audit { }` in a session every later observation
was retained until exit — and because `active_policy` answered `Some` while
the trail stood open, every later pipeline stage inherited a policy and
shipped an `AuditFragment` home. The delimited lifecycle kills both
structurally: close is what turns stage inheritance back off.

## Decision

### The mechanism, stated once

`delimit : Policy × Extent → Ending × Trace`, under five laws:

1. **Flat merge.** Observations land flat in whichever trail is open; an
   inner delimiter's entries remain visible to every outer one. (Today's law,
   kept.)
2. **The opener owns closing.** `Audit::open` returns a `TrailScope` — not
   `Clone`, not `Copy` — recording whether it installed the trail or found
   one open; `Audit::close` spends it: an opener drains the trail to `None`
   on every exit, panics included; a nested scope copies its suffix and
   leaves the trail intact. `delimited` (`core/src/evaluator/audit.rs`,
   replacing `forced_subtree`) closes under `catch_unwind` and resumes the
   unwind after.
3. **Facts are authored where they commit.** The engine's doors observe
   engine-side; the desk observes host-side, at the commitment arm; helpers
   ship fragments. Authors are plural, the vocabulary is one.
4. **The projection is total.** `Observation::to_wire` and the shared
   `serial::placeholder` pass replace every leaf beyond first order with a
   tagged `` `opaque {type: …} `` variant — never a bare string, so no
   genuine string can impersonate it — and never drop the observation. The
   rail sink's silent drop-with-`dbg_trace` (`Mooring::surface`) is retired;
   the fragment wire placeholders a `Handle` before interning while closures
   stay rich, interning against the fragment's scope table and decoding back
   live — that seam's advantage over a flat wire.
5. **The trail carries facts, never prose.** Past tense, remedies, ordering —
   all of it belongs to renderers, of which each audience gets exactly one.

### Four delimiters, two authors

| client | delimiter | policy | reads |
|---|---|---|---|
| `audit { }` | `eval_audit` via `delimited` | `Bytes` | its subtree, as `tree_value` |
| `try { }` | `eval_try` via `delimited` | `Off` | its subtree, to name the failing command |
| `ral --audit` | the session (`enable_audit` opens, `take_audit_fragment` drains) | `Bytes` | everything, at each drain |
| exarch tool call | the dispatch (`Run.trail: Some`, scope held at `Shell::enter`) | `Off` | its extent, on `Report::Ran.trail` |
| desk acts | none — an *author* | — | contributes the host-side `ActFragment` |
| sandbox / pipeline helpers | none — *authors* | inherited | contribute `AuditFragment`s (unchanged) |

The session opener is the one client whose extent has no close: it stays open
for the process's life and is drained per batch. The dispatch is the one
client that cannot express its extent as a closure: `Shell::enter` holds its
`TrailScope` *outside* the `catch_unwind` — load-bearing, because the
`Mobile` checkpoint the panic arm rolls back does not cover `local.audit`, so
only a scope the panic cannot skip keeps law 2 true at dispatch granularity.
The capture merge (`merge_capture`) stays monotonic, so a dispatch's `Off`
nests inside a `--audit` session's `Bytes` without silencing it.

For exarch the marginal cost is retention only: full observations are already
constructed per command for the rail whenever a surface sink is installed —
which every tool call installs — then dropped. `CapturePolicy::Off` retains
them for one dispatch instead; no byte tee.

### The desk records host-side, never into the engine's trail

`Observed::Act { verb, subject, payload, refused }` is the host-authored
variant. `HostServices::commit_act` constructs **one** `Observation` per
attempt, at the arm where the outcome is known, and fans it out itself: the
rail row (`Kind::HarnessCall`) always, the per-call fragment only when
committed — the ledger's law kept, since the fragment answers *what stands*
and a refusal already raised in-band. "A seventh act cannot reach one reader
and miss the other" stops needing enum adjacency and holds by construction.
The spawn verb's second author — `tools/agent.rs`'s pre-thread `HarnessCall`
emission, which drew a non-refused row for a spawn that might fail to start —
dissolves into the same commitment-arm fan-out.

Where the desk records is as deliberate as what: on a wire seat a cancel
landing while the engine is parked in `enquire` can unwind a builtin whose
act the desk already committed, so engine-side recording alone would report a
standing act as failed. Host-side, at commitment, the fragment is correct
regardless of how the engine's frame dies. Engine-side the harness builtins
still produce ordinary `Command` observations through `frame_call` — journal
facts, never deduplicated against the fragment's commitment record; the audit
sentence reads only the fragment, the journal reads only the trail.

### Worker births are presence-in-trace

`Observed::Worker { id, cmd, class }` is pushed through `observe` at the one
spawn door, `spawn_child` — after the reservation succeeds, never before, so
a cap-refused spawn observes nothing and no phantom birth reaches the orphan
join. This is the entire replacement for `birth_epoch` and the `` `workers ``
probe's `born-this-epoch` field: membership-in-extent becomes presence in the
dispatch's own trail, and the documented misattribution leak — a grandchild
spawned from inside a running worker, stamped to whatever dispatch was
current — vanishes structurally, since a worker's own extent has its own
mooring and its births were never the foreground dispatch's to claim.
`audit { spawn … }` gets richer for free: the birth appears among its
children, id and all.

### The trail rides the Report, unbounded

`Run.trail: Option<CapturePolicy>` asks; `Report::Ran.trail: Vec<FOValue>`
answers, each observation projected through `to_wire`. Not the event stream:
the engine already streams every observation live as surface events, but the
stream is the *live* rendering — exarch decodes to rail rows and drops them —
and accumulating it host-side per dispatch would be exactly the
re-materialization pattern this ADR deletes. The Report is the dispatch's one
terminal fact; "what this dispatch did" belongs in it.

No bound, deliberately — the law `captured` already obeys: the Report carries
facts verbatim, every model-facing bound lives in the renderer, and the
wire's frame fuse (`MAX_FRAME_LEN`) is the shared backstop. Worker births
land *early* in an extent, so a tail-keeping cap would silently evict exactly
the entries the orphan join needs.

A recovered panic is excluded from the central law by declaration: it reports
`Static` — diagnostics only, no trail, no ending arm — because a panic is a
harness defect, not a program ending. The scope held at `enter` drains on the
panic arm and discards; what survives is what lived outside the engine — the
desk's host-side fragment still renders, the registry still answers the
probe — and only the births' trail attribution dies with the panicked
dispatch.

### One renderer

`shell_eval::report::render(ending, trail, fragment, workers, timeout_secs) →
(stderr suffix, exit)` is the single model-facing composition, pure — the
`` `workers `` probe is read by the caller at the run boundary and handed in
as rows. It composes, in code and in this order: the engine's rendering
verbatim; the ending's remedy (timeout tip / non-zero-exit tip); the audit
sentence (a fold over the fragment, `DeskAct::done` supplying the past
tense); the orphaned-work sentence. The two-layer stderr appends collapse
into it; the byte-offset ordering test dies because order is now code.
`ToolResult` loses `timed_out` — the renderer reads the wall's verdict from
the `Ending` tag and answers 124 itself.

**Orphans, deliberately widened.** The trigger is binding loss, not the wall:
a raise, the wall, an `exit` — the handle is equally lost on a routine
non-zero exit. `Stopped` draws nothing, because job control is no ending and
keeps bindings; `Settled` has nothing to answer for. The renderer joins the
trail's `Worker` births against the probe by `id` and names those still
present in the registry — running *or* settled-unclaimed, both equally
unreachable once the handle binding unwound; a consumed worker has left the
registry and is nobody's orphan. The first `NAMED` are named, the rest
counted aloud. The noise is bounded by construction: the join keys on *this
dispatch's own births*, so a failing dispatch that spawned nothing says
nothing — no false positives exist to budget for; the widening only surfaces
true orphans earlier.

### The ending is a sum — two of them, honestly

`Report::Ran`'s flat product (`result: Result<FOValue, Break>`, `status`,
`single_command`, `timed_out`) could represent `Ok` beside `timed_out: true`;
the correlation was convention. `Ending` makes it literal — and it shipped as
**two types**, stratified along the pre-existing `RunReport`/`Report` seam:

- **`run::Ending`** (`core/src/run.rs`), engine-side: `Settled { value:
  Value, status }`, `Raised`/`Walled` carrying a live `Error` plus
  `single_command`/`root`, `Exited(i32)`, and (unix) `Stopped { pgid, signal,
  cmd }`. `classify_ending` folds the settled parts; `Ending::status` and
  `Ending::into_result` recover the flat views.
- **`transport::Ending`**, wire-side: the same arms with the error *rendered*
  against the `SourceDb` — `Raised { rendered, command_exit, single_command,
  status }`, `Walled { rendered, status }` (no `command_exit` — the timeout
  remedy is unconditional, so no renderer reads it there), `Stopped` with a
  `signal_name` string.

`render_ending` in `RunReport::into_report` is the one lossy projection
between them, at the last point the engine's `SourceDb` is in hand.
`transport::Break` dissolves into the arms. The plan's open question —
whether `command_exit` and `single_command` both earn their seat once the
renderer is the one consumer — settled *yes*: `command_exit` gates the
exit-code remedy, `single_command` picks between its two wordings. One
`PROTOCOL_VERSION` bump, 5 → 6, covers the whole batch (the plan scheduled
two; the waves landed in one sitting, so the shapes moved together).

### Rider: `schedule`'s label is mandatory

Unifying the desk's recording onto one `Observed::Act.subject` field feeding
both readers exposed a conflict only the minted `sched-<n>` default could
cause: the rail wanted the caller's own label, the audit wanted the label the
registry resolved, and they disagreed exactly when the caller supplied none.
The repository owner ruled during implementation that `schedule` always takes
a label from the model: `ScheduleRegistry::schedule` takes a plain `String`
and mints nothing, `payload_label` refuses a missing or malformed label at
the wire, and the reserved `sched-<n>` shape retires with nothing left to
protect. The full reasoning lives as amendments on
[[decisions/260719_agent-names-and-schedule-labels|agent-names-and-schedule-labels]]
and [[decisions/260720_harness-calls-are-acts|harness-calls-are-acts]].

## Alternatives considered

- **A tail-keeping cap on the Report's trail.** Rejected: worker births land
  early in an extent, so any tail cap could silently evict precisely the
  entries the orphan join needs — a bound that starves the one consumer it
  was meant to protect. Retained `value` weight is accepted on the same
  reasoning: the entries hold values the rail already builds, and the
  delimited close drains them per dispatch. Revisit only on evidence — the
  symptom would be VM memory pressure or a refused frame, never a wrong
  trail.
- **Accumulating the event stream host-side per dispatch.** The engine
  already streams every observation live; rebuilding the extent from that
  stream — buffering rail rows, disentangling deferred worker batches
  interleaved as `SessionEvent`s — is exactly the per-stratum
  re-materialization this ADR deletes. The Report is the dispatch's one
  terminal fact.
- **Recording desk acts engine-side, in the trail like every other fact.**
  Rejected for the wire seat: a cancel landing while the engine is parked in
  `enquire` can unwind a builtin whose act the desk already committed, so the
  engine's frame would report a standing act as failed. The desk stays a
  host-side author into a per-call fragment joined at render.
- **For the schedule rider: let one reader's label win.** Taking the caller's
  (blank when unlabelled) starves the audit of the only name the schedule is
  known by; taking the registry's resolved default puts a string in the rail
  the caller never wrote. One `subject` field cannot hold two strings;
  requiring the label dissolves the conflict at its source instead of
  arbitrating it.

## Consequences

- The trail leak is dead, structurally: a `try` or `audit { }` that opened
  the trail closes it on every exit, panics included, so a pipeline stage
  launched afterward inherits no policy and ships no fragment.
- The dispatch's journal exists: every exarch tool call asks with
  `Some(CapturePolicy::Off)` and receives its extent's trail on the Report; a
  cancelled dispatch's Report carries the prefix including the struck
  command. The REPL passes `None` and pays nothing.
- Every seam is total: a handle-bearing observation reaches the rail, the
  Report, and the fragment wire as its tagged `` `opaque `` placeholder
  instead of dying or vanishing; closures stay live across the fragment seam.
- `birth_epoch`, `WorkerRegistry::epoch()`, `` `born-this-epoch` ``,
  `surviving_worker_note`, `ActLedger`, `struct Act`,
  `ToolResult.timed_out`, `transport::Break`, and both stderr append sites
  are deleted; `evaluator/audit.rs`'s "host doors have no trail" exception is
  gone — the desk is an author, not an outsider.
- The orphan sentence fires on any binding-discarding ending, naming only
  this dispatch's own births still present in the registry; `Stopped` draws
  neither the audit nor the orphan sentence.
- The wire moved once: `PROTOCOL_VERSION` 6 — `Run.trail`,
  `Report::Ran { ending, captured, trail }`, the probe row minus
  `born-this-epoch`, and the mandatory schedule label.
- What deliberately stays: `WaitOutcome::Cancelled` and the teardown ladder
  (attribution of an OS death is evidential, done where the evidence is);
  ready-boundary notices (session facts, not effects of any dispatch's
  extent); the `` `workers `` probe as the quiescent-state read, minus the
  membership hack.

## Open questions

- The trail's `value` weight is unbounded by declaration; the revisit trigger
  is evidence (VM memory pressure, a refused frame), never a wrong trail.
- The session opener (`enable_audit`) still speaks `install_active_policy` /
  `take_audit_fragment` rather than holding a `TrailScope` of its own — sound,
  since the session's extent has no close, but it is the one opener outside
  the scope-shaped API.
- A panicked dispatch loses its births' trail attribution with the discarded
  trail; the registry still answers the probe, so the workers are findable,
  just no longer attributable. Accepted, not solved.

## See also

[[design/audit|audit]] (the design page this ADR's lifecycle and dispatch
delimiter now live in), [[internals/a-turn-end-to-end|a run, end to end]]
(where `classify_ending`, `Shell::enter`'s scope, and `render_ending` sit in
the run spine),
[[decisions/260720_harness-calls-are-acts|harness-calls-are-acts]] (the act
vocabulary this ADR re-founds on `Observed::Act`, and the schedule-label
amendment in full),
[[decisions/260719_agent-names-and-schedule-labels|agent-names-and-schedule-labels]]
(the `sched-<n>` default retired by the rider),
[[decisions/260726_cancel-is-a-join|cancel-is-a-join]] and
[[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]] (the unwind
whose surviving prefix this ADR reports),
[[decisions/260722_session-is-a-process|session-is-a-process]] (the wire seat
whose parked `enquire` fixes where the desk records),
[[map/exarch/shell-eval|shell-eval]] and [[map/core/shell-state|shell-state]]
(the maps that lost the epoch paragraphs).
