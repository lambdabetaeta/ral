---
status: active
---

# A handle is the evidence of detachment

**Given a turn evaluator with a durable *root* cancel scope distinct from the
swappable *foreground* scope, a worker's parent follows the primitive that
creates the worker, and a handle is the public evidence that the worker is
detached.** `spawn` and REPL-only `watch` reify a `Value::Handle` and hang their
workers at the root. Pipelines create no handle and remain foreground work.
`par` is the compromise: it returns no handle and joins within the expression on
the success path, but it is prelude code over `spawn`, so its fan-out is
root-parented rather than nurseried; an early exit can leave the unjoined tail at
the root.

## Context

This rests on [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]:
the turn evaluator has a durable **root** `CancelScope` on `Shell`
(`shell.session.root`) that no turn swaps, distinct from the swappable
**foreground** scope (`shell.turn.cancel`). The 30 s wall and host-routed user
interrupt cancel the *foreground* scope; the root survives. Plain
process-global SIGTERM/SIGHUP and batch/Windows SIGINT still trip
`SIGNAL_COUNT` in every polling thread; those are outside the foreground-scope
path this ADR relies on. The root split fixes the collateral kill — a
background worker dying because the foreground turn that launched it timed out —
by parenting detached workers at the root. The open question this ADR answers:
**given the split, how are the primitives recast?**

The source framing still needs this distinction. The module header of
`core/src/builtins/concurrency.rs:1` labels the whole set "**structured**
concurrency primitives" — yet `spawn` and `watch` produce handles that outlive
the turn and are observed on later turns. `Shell::spawn_thread`
(`core/src/types/shell/inherit.rs`, ~line 195) takes a child of
`shell.session.root`, and its sole live caller is `spawn_child`
(`concurrency.rs:88`) — the path `spawn` and `watch` share. That makes the
scope edge explicit for handle-producing thread workers: the public handle marks
detachment, and the worker's cancel scope hangs at the root.

A grounding correction the code forced: the
pipeline does **not** flow through `spawn_thread`. Its stages are OS processes
in a `PipelineGroup`, and current launch code passes the active foreground
scope straight through (`core/src/runtime/pipeline/launch.rs:141`) rather than
minting a pipeline child scope. Making that edge explicit is cutover work from
the sibling ADR, not a current fact. `par` is **prelude code over `spawn`**
(`core/src/prelude.ral:148`), so its workers *are* `spawn` workers — detached by
parenting, joined by sequential `await` on the success path. A foreground
deadline, a scope-routed interrupt, or a failed `await` cuts that join short and
leaves the unjoined tail root-parented. The detached/structured line is
therefore drawn at the **thread-spawn primitive** (`spawn`/REPL `watch`, the
only `spawn_thread` users) and at **join discipline** (does the launching
expression `await` every handle before returning?), not at a single uniform
scope choice.

## Decision

The organising principle and the per-primitive recast:

- **The handle is the evidence of detachment, not the whole story.** A primitive
  that returns a `Value::Handle` parents its worker at the **root**. A primitive
  that creates no handle and owns its children directly stays in the
  **foreground**. `par` is explicitly the exception left as library code over
  `spawn`: structured result, detached workers.

| primitive | reifies a handle? | parent scope | lifetime |
|---|---|---|---|
| `spawn` | yes | root | completion · `cancel` · exarch one hour ceiling if armed · root abort / session exit |
| `watch` | yes, REPL frame only | root | completion · `cancel` · root abort / session exit |
| pipeline `\|` | no (OS process group) | foreground; pipeline child scope is cutover work | joined before the turn returns |
| `par` | no (joins its `spawn` handles) | root (its workers are `spawn` workers) | `await`ed in input order on success; foreground cancel or first failed `await` can orphan the unjoined tail |
| `await` | — | runs in the foreground | wall-bounded wait |
| `poll` | — | — | total, non-blocking |
| `race` | — | runs in the foreground | wall-bounded wait; loser cancellation is explicit handle cancellation |
| `cancel` | — | — | the explicit handle reaper |
| `forget` | — | — | **recommend deletion** |

- **`spawn` — parent at the root (detached).** It acquires the host's detached
  worker policy: exarch arms a one hour lifetime ceiling; the REPL does not.
  Cross-turn persistence becomes defined behaviour, not the happy-path accident
  the sibling ADR documents for the REPL.

- **`watch` — root-detached, but REPL-only.** A watched worker writes to a live
  host output sink as it runs. In the REPL that sink is durable
  (`rustyline`'s external printer) and can outlive the turn. In exarch the
  active stdout/stderr sinks are per-call capture buffers; a root-surviving
  watcher would keep writing into a stale buffer after `run_shell` had already
  taken it. The exarch frame therefore does not admit `watch`. Detachment may
  hold only root-owned or handle-owned resources, never a foreground frame's
  capture state.

- **pipeline `|` — bounded by the foreground.** Semantics unchanged, but the
  implementation cutover must make the scope edge explicit: stages are processes
  in one group under a foreground child scope owned by the pipeline, reaped by
  group-terminate when the foreground is cancelled (deadline / foreground
  interrupt). This is the cancel→drain→collect path the sibling ADR names;
  `RunningChild::observe`'s typestate already enforces wait-before-drain.

- **`par` — keep the prelude compromise.** `par` spawns each item with
  `spawn { … }` and `await`s the handles in input order before returning
  (`prelude.ral:148`). The workers therefore hang at the **root**, not the
  foreground. On the all-success path the *expression* is structured even though
  the *workers* are detached. Under a foreground deadline or scope-routed
  interrupt, a `par` in flight unwinds like any blocked `await`, and its
  root-parented fan-out survives. If an awaited worker fails, `await` re-raises
  and the same early-exit rule leaves later handles unjoined. In exarch the one
  hour lifetime ceiling reaps those orphans. In the REPL there is no ceiling, so
  a REPL foreground Ctrl-C can leave them until explicit `cancel`, root abort
  (Ctrl-`\`), or session exit. That orphan window is accepted to keep `par`
  ordinary prelude code rather than adding a foreground-parented nursery spawn
  path.

- **`await` — becomes cancellation-aware, sharing one wait loop with `race`.**
  Current `await_handle` (`concurrency.rs:284`) blocks on a bare `rx.recv()`.
  That receive does not poll `process::check`, so a root-scoped worker that
  outlives the foreground can keep `await` asleep past the wall. `race`
  (`concurrency.rs:354`) already does the right thing:
  `loop { try_settle; process::check?; sleep(1ms) }`. **Unify them.** `await`
  blocks in the foreground (wall-bounded); if the wait is cut short by the wall
  or a foreground-scope interrupt it unwinds via `process::check`, but the
  **root-scoped worker survives** to be observed on a later turn. This dissolves
  both the bare-`recv()` hang and the collateral kill.

- **`poll` — unchanged, promoted to the primary cross-turn observation.** It is
  already total, non-blocking, and wall-friendly
  ([[decisions/260615_poll-total-failed-arm|handle-settle]]). The canonical
  idiom for a long-running agent: **`spawn` → `poll` across turns → `await` once
  settled** (which then returns instantly off the cached `CompletedHandle`).

- **`race` — mechanism unchanged.** It is the cancel-aware loop `await` adopts;
  it cancels the losers with explicit handle cancellation and projects the
  winner.

- **`cancel` — unchanged surface, explicit cause internally.** With detachment
  the default, `cancel` is how a detached worker is stopped before its ceiling:
  it fires the worker's scope with an explicit handle-cancel cause and detaches
  the handle.

- **`forget` — RECOMMEND DELETION.** Its current behaviour (`builtin_forget`
  `concurrency.rs:217` → `detach_handle` `concurrency.rs:458`): it sets state
  `Forgotten`, drops the result receiver, and clears the cache — but it does
  **not** touch `handle.cancel`, so it never stops the worker. With `spawn` and
  admitted `watch` already rooted at the durable root, this is no longer
  detachment; it is "discard the observation channel while letting the worker
  run." That is not identical to merely delaying `await`: retaining a handle
  preserves later `poll`/`await`/`cancel`, while a forgotten handle is
  intentionally unobservable (`ensure_live`, `concurrency.rs:228`, makes
  observing one an error). The only residual surface is an explicit
  discard-result operation, and the ordinary way to express that is to stop
  retaining the handle. The extra verb earns no behaviour worth preserving;
  record the recommendation as deletion.

## New behavior: detached worker lifetime is a frame policy

The one genuinely new mechanism for exarch, and the management story for
abandoned detached workers there: a **one hour lifetime ceiling**
("death-clock") the exarch frame arms on every detached `spawn` worker. A worker
that has neither completed nor been cancelled within one hour is
force-reaped. There is no listing, inspecting, or enumerate-and-kill primitive
for detached workers — a worker runs, a live handle may cancel it, and if exarch
abandons it the ceiling eventually kills it. Without the ceiling an abandoned
`spawn { loop { … } }` is an immortal zombie hanging at the root, unreachable
once the agent loses the handle binding across context compaction; the ceiling
is what makes detachment safe for a long-running agent.

- **A frame policy, like the foreground wall.** The ceiling is supplied by the
  frame (the sibling ADR's missing type), not baked into core: the **exarch**
  frame arms a one hour ceiling on each detached `spawn` worker so an agent
  cannot accumulate zombies; the **REPL** frame arms none, leaving an interactive
  worker to `cancel`, root abort, or session exit. `watch` is REPL-only and
  therefore has no exarch ceiling. A worker's lifetime is set by its host, not
  the model.
- **A capped default, not a per-spawn knob.** One hour is deliberately longer
  than exarch's 30 s foreground wall (`CALL_TIMEOUT_SECS`,
  `exarch/src/tools/ral.rs:31`) but still bounded enough for abandoned fan-out
  and lost handles. There is no per-call dial.
- **The reaper is the worker's own scope, fired by the shared timer service.**
  A worker stores its `CancelScope`; reaping it is a `(scope, now + 1 hour)`
  entry in the one timer/reaper service the sibling ADR introduces for the
  foreground wall — not a watchdog thread per worker, which thread-per-`spawn`
  would require and not scale. At expiry the service flips the scope with
  `Deadline`; the worker
  unwinds at its next `check`, and its child-wait loop SIGTERM→SIGKILLs any
  subprocess group, exactly as the wall reaps the foreground.

## Consequences

- The detached/foreground line makes "does this worker survive the foreground
  turn?" answerable by reading the construct: `spawn`, REPL `watch`, and `&`
  survive; pipelines do not; `par` joins on success, but its implementation
  fan-out is detached and can orphan on foreground early exit or worker failure.
- `await` no longer hangs past the wall and no longer collaterally kills the
  worker it awaits — the two failure modes the bare `recv()` produced both go
  away once `await` shares `race`'s cancel-aware loop and the worker lives at the
  root.
- Deleting `forget` removes a primitive whose name lies. The lost surface is
  only explicit "discard this result while the worker keeps running"; retaining
  a handle already preserves observation and cancellation, and dropping the
  binding already expresses fire-and-forget.
- Exarch detached workers can no longer leak forever: the one hour frame
  lifetime ceiling reaps abandoned `spawn` workers. REPL workers remain
  interactive session state and die by `cancel`, root abort, or session exit.
- `par` keeps its semantics, but its workers are now documented as root-parented
  — a fact a reader would otherwise have to derive from the prelude.
- **Detached workers are unmanaged by design — there is no introspection
  primitive.** No `jobs`-for-handles, no enumerate-and-kill: an abandoned exarch
  worker is reaped by the lifetime ceiling, not found and stopped by hand. The
  REPL `JobTable` covers stopped foreground process groups, not `spawn` handles.
  `jobs`/`fg`/`bg`/`disown` therefore gain no spawn-handle analogue.

### What shipped

The recast landed on `turn-local-state`.

- **`forget` deleted** — the builtin, the `HandleState::Forgotten` state, and
  every arm that handled it. `detach_handle` now only ever transitions to
  `Cancelled`; dropping a handle binding is the fire-and-forget gesture.
- **`await` is cancel-aware**, sharing one `wait_first_settled` loop with
  `race`: it blocks in the foreground polling `process::check`, so a deadline
  or interrupt unwinds the wait, but the root-scoped worker survives. The
  bare-`recv` hang and the collateral kill are both gone, pinned by
  `await_unwinds_on_foreground_cancel_sparing_the_worker`.
- **The detached-worker policy is a frame axis.** It rides `TurnFrame` into
  `TurnState`, flowing down through `inherit_from` and into workers through
  `spawn_thread`. The REPL arms no ceiling; exarch arms a one-hour ceiling.
  (The axis first shipped as `DetachedPolicy { lifetime, watch }`;
  [[decisions/260617_watch-repl-builtin|watch-repl-builtin]] then collapsed it
  to the bare `detached_ceiling: Option<Duration>` once `watch` admission left
  the frame.)
- **`watch`'s host-specificity** — once a frame gate refusing under capture
  buffers — is now registration-by-absence: `watch` is a host-installed
  builtin (the ral binary installs it, exarch does not), per
  [[decisions/260617_watch-repl-builtin|watch-repl-builtin]].
- **The death-clock rides a shared reaper.** `process::reaper` ("deadlines as
  data") is one process-global daemon holding `(when, scope)` entries; `spawn`
  arms `(worker_scope, now + 1 h)` under the exarch frame as a kept,
  fire-and-forget entry. This is the single timer/reaper service
  [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] named but
  left unbuilt — built here.
- **exarch's foreground wall moved onto the same reaper.** `run_shell` arms its
  30 s deadline as a *disarmable* reaper entry rather than a watchdog thread per
  tool call, and the guard disarms it when the turn finishes early. The wall and
  the death-clock are now one service, differing only in scope, `when`, and
  whether the entry is kept (the death-clock) or disarmed on completion (the
  wall).

**Not adopted.** The explicit pipeline-child scope is still cutover work —
stages share the foreground scope. `par` is unchanged prelude code over
`spawn`, but its fan-out now inherits the frame's detached policy through
`spawn_thread`, so a `par` under exarch is ceiling-armed like any other `spawn`.

## Alternatives considered

- **Keep one foreground parent for every worker (the rejected collateral-kill
  shape).** Rejected: this is exactly the bug. Every worker a child of the
  swappable foreground (`shell.turn.cancel`) means a foreground timeout reaches
  a worker meant to outlive the turn. A boolean flag on `spawn_thread`
  ("detached: bool") was the obvious patch and is rejected for the same reason
  the sibling ADR rejects flag-soup — the *handle* already carries the
  distinction, so the scope choice should read off it, not off a parallel
  parameter.
- **Keep `forget`.** Two ways to keep it were weighed. (a) Redefine it as an
  explicit *discard-result* marker — but after that call the same binding cannot
  `poll` or `await`, while dropping the binding already expresses "do not
  observe this result" without poisoning the handle state. (b) Reserve the name
  for a future reachability-GC (a worker whose handle is no longer reachable is
  reaped) — a real idea, but it is a different mechanism with no implementation
  here, and squatting the name now is premature. Rejected: delete it; revisit the
  name if reachability-GC is ever built.
- **A foreground-nursery `par`.** Rejected for now. It would require a
  foreground-parented internal spawn path solely for prelude `par`; keeping `par`
  ordinary library code over `spawn` is simpler, and the orphan window
  (foreground deadline, scope-routed interrupt, or worker failure) is bounded in
  exarch by the lifetime ceiling. In the REPL the tradeoff is visible: a
  foreground Ctrl-C during `par` can leave root workers until explicit `cancel`,
  root abort, or session exit.
- **`watch` in every host.** Rejected: a detached watcher must write only to
  root-owned or handle-owned resources. exarch's stdout/stderr buffers are
  foreground-frame resources collected at turn end, so a root-surviving watcher
  would write into stale capture state. `watch` is admitted only by a frame that
  supplies a durable watch sink; today that is the REPL.
- **A per-spawn deadline knob** (`spawn { … } timeout: 5m`). Rejected in favour
  of the capped default: ordinary `spawn` stays one boring primitive with
  host-owned lifetime policy. A legitimately long-lived exarch job should be a
  second, explicit mechanism — for example a promotion/disown operation into a
  host-managed durable job — rather than a timeout dial hidden on every spawn.
  That mechanism is future work and must not be confused with the REPL's POSIX
  `disown`.

## Open questions

- **Long-running exarch work.** One hour is the default detached-worker
  ceiling. If exarch becomes a genuinely long-running agent, some work may need
  to outlive that bound. Do not solve that by adding a timeout knob to ordinary
  `spawn`; design a second mechanism that intentionally promotes a handle into a
  host-managed durable job, with its own ownership, listing, cancellation, and
  lifetime policy. `disown` is a possible spelling, but the REPL already uses
  that word for POSIX job control, so the eventual surface needs care.
- **The kill-all gesture** — what cancels the durable root, given
  foreground-scope interrupts cancel only the foreground — lives in
  [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]. Referenced,
  not duplicated.

## `&` is `spawn`; the `JobTable` stays separate

`cmd &` is not a second parallel backgrounding mechanism — the elaborator
desugars it to `spawn { cmd }` (`core/src/elaborator.rs:578`, "pure sugar"). So
`&` inherits this ADR's handle model wholesale: root-parented, detached,
surviving the command, reaped by completion / `cancel` / the host lifetime policy
/ root abort or session exit. There is no second "task" vocabulary to reconcile.

The Unix `JobTable` (`jobs` / `fg` / `bg` / `disown`, see
[[map/repl/jobs|repl/jobs]]) is **not** a management layer over `&` or `spawn`.
It tracks foreground process groups that the kernel stopped and the evaluator
reported as `Escape::Stopped`; `ral/src/jobs.rs` explicitly says an
`&`-backgrounded pipeline is an in-process `spawn` handle and is not tracked
there. This keeps the language/exarch rule intact: there is no introspection
primitive for detached handles. If ral ever grows POSIX-style job-control
backgrounding distinct from `spawn`, that is a separate feature, not this ADR.

See also [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] (the
premise: the root/foreground split), [[decisions/260615_poll-total-failed-arm|handle-settle]]
(the settle `poll`/`await`/`race` project),
[[decisions/260504_hot-path-cancellation|hot-path-cancellation]] (where workers
poll the scope), [[design/scoping|scoping]], [[map/core/builtins|map: builtins]],
[[internals/output-capture-and-detachment|output-capture-and-detachment]] (how
this plays out for a never-closing pipe — the long-running-server flow), and
`docs/SPEC.md` §13.3.
