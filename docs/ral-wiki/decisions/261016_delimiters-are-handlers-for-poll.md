---
status: active
generated_at_commit: dad6a1b3
verified_at_commit: dad6a1b3
anchors: [Escape::Stopped, Break::stopped, Break::of_child, final_poll, ThreadEnd, Shell::poll, Shell::scope, Mooring::home, Scope::in_force, ScopeId, POLL_SLICE, WorkerStop, Status::WorkerStopped, convert_own_stop, StageObservation::names, CollectState::cancel_all, CollectState::recv, Ending::Stopped]
---

# Delimiters are handlers for `poll`

> Amended 2026-10-23: the sentinel drains a dead edge for as long as it is
> held, so an external that a reader-gone stage's cleanup runs does not block
> on the edge but runs to completion within the grace, its output discarded,
> and a teardown reads, unmarked, every edge a reader has left (SPEC §7.6).
> Law 6's parenthesis and the Consequences bullet "An external in a
> reader-gone stage's cleanup …" no longer hold.

> Amended 2026-10-20 by [[decisions/261020_gather|gather]]: in the construct
> table, the `par` row is the `gather` row. `gather B` — and with it `par`,
> `both` and `first-of`, prelude code over it — cancels and drains the
> children it owns on every halt of its body, a failure, an `exit` or a stop
> alike, then passes the halt; a panic cancels them without waiting. `par` is
> prelude code again, and still cancels and drains its siblings on every
> exit. `all` is deleted. Law 9 and `convert_own_stop` are unchanged: a
> gather is not a worker base and converts nothing.

> Amended 2026-10-19 by [[decisions/261019_await-is-the-block-run-here|await-is-the-block-run-here]]:
> a worker's base consumes every `Cancelled` or `TimedOut` stop whose owner
> is not the session root, not only one naming its own node, so "consumed iff
> it names that owner's node" (the Decision's owners bullet, law 9) no longer
> holds for workers; root causes and `Interrupted` still pass. In the
> construct table, `await h` and `race` behave as the block run here: they
> replay its bytes, then end as it ended, so the row's re-raises stand, "own"
> in its stop column now means any such in-session stop, and `race` drains
> its losers before it returns. The `par` row is replaced: `par` and `all` are builtins that own their children,
> and on a failure, an `exit`, a stop or a panic they cancel and drain every
> sibling before re-raising. "`par` stays prelude code" is retired, and with
> it the rejection of `par` as a Rust builtin and the consequence that `par`
> no longer cancels its siblings on Ctrl-C or a wall: the nursery this page
> deferred is the call-owned birth path.

**A stop is an escape with an owner, not a catchable error.** A computation
ends in `return V` or `halt σ`, and a signal σ has two sorts, told apart by
who may consume it: an *error* is unlabelled and the nearest `try` takes it;
an *escape* is consumed by one owner and passed by every other frame. A
cancellation raises a **stop**, the escape `stop ν c`: its cause `c` chooses
its message and status and nothing else, and its name `ν` is the scope node
that owns it. Cancellation is one operation, `poll`, and every delimiter that
has an opinion about it — `try`, a worker's base, a pipeline's collector, a
`guard` cleanup's shield, the top of a run — is a handler for it. The binary
used to spell a stop as `Break::Error` with `Status::Cancelled`, and every
defect in the area was one face of that spelling.

## Decision

- **The sort.** `Escape ::= Exit n | Stopped { cause, owner: Option<ScopeId>,
  at }`, minted only by `Break::stopped`. `Error`'s statuses are `Raised n`,
  `Process f` and `WorkerStopped o` (`o : WorkerStop ::= Cancelled |
  TimedOut`), the last being a stop an owner *converted* at its boundary; no
  poll point mints it. `Reason` is flat: `` `exited n | `signaled n |
  `cancelled | `timed-out | `not-found | `not-runnable | `raised ``. `try`
  catches errors and nothing else, as the Agda's `βtry-err`/`βtry-esc` already
  said.
- **Three mechanisms.** *Frames*: the CEK machine's stack holds the handlers;
  `poll` is `Shell::poll`, performed at every step boundary (β, bind, apply,
  exec, the iterating builtins, pipeline launch, the worker-join sweep, a
  base's closing poll). *The tree*: scope nodes `{ flag, cancelled_at, parent,
  shield }`, minted at exactly six points — the session root, a worker's node
  and the session anchor, a host's dispatch scope, a run's node, a stage's
  node (under the *current* scope), and a cleanup's shield. *Sliced waits*:
  every blocked operation of ral's own — a pipe write or read, a child's wait
  and its pumps' drain, the collector's receive, the enquiry park, the `await`
  sweep — is a sequence of (read the scope; wait at most `POLL_SLICE`, 10 ms).
  The environment flips a flag; every thread samples its chain at least every
  slice. Nothing is delivered, so nothing can be lost.
- **Two cells.** The *owner's node* is run-invariant and is what writes aim
  at: `Mooring::home` (a key the terminal tenant heard, a lease, a `cancel`,
  a stage's group membership). The *current scope* is a descendant swapped by
  frames and is what reads consult: `Shell::scope`. At a base they coincide;
  inside a cleanup the scope is a shield under the node. Nothing ever writes a
  shield: `Mooring::home` is a `ScopeNode`, `Shell::scope` a `Scope`, and a
  shield exists only as a `Scope`.
- **One writer per node.** The root by SIGTERM/SIGHUP/Ctrl-`\`; the dispatch
  scope by the host's interrupt; a run's node by its wall and the heard key; a
  worker's by `cancel`, a `race` loss, its lease, the session wipe; a stage's
  by its collector alone, with the cause of whatever teardown it runs; a
  shield by nobody.
- **The name.** `j(n)` is the join of the flags on `n`'s chain; `ν(n)` is the
  *outermost* node whose own flag attains it — a declared tie-break, not a
  record of who struck first. So a cause the collector writes on a stage that
  an ancestor already carries renames nothing.
- **Owners are base frames.** A run's top, a worker and a stage each run
  `evaluate` under a boundary that catches a panic and applies one return
  clause, which opens with a **closing poll** (`final_poll`): the node is read
  once more after the body settled, so a stop that landed after the last poll
  is not lost. A stop at a boundary is consumed iff it names that owner's node.
  A worker consumes a stop naming its own node into the handle's error
  (`convert_own_stop`, `Error::worker_stopped`). A stage forwards everything;
  its *collector* consumes every stop naming that stage's node — its reader-gone
  stop, and the teardown the collector itself ran. A direct external stage has
  no node; the collector's record of what it sent (`sent`) is its name.
- **The top is no owner.** Every node above a worker's or a stage's is the
  host's, so a stop naming one, or none (the closed-stdout stop), is a
  **top-level effect**: no frame of the program interprets it, and the host
  reads it off the empty stack as `Ending::Stopped`, as it reads `exit n` as
  `Ending::Exited`. `Ending::Walled` is gone: a wall is a `TimedOut` stop.
- **Level-triggered.** The escape is the vehicle; the condition is the node's
  flag, which only rises. Consuming a stop clears nothing, so the next `poll`
  outside the consumer reads it again.
- **The pipeline verdict.** A consumed stop contributes nothing, and so does a
  reader-gone stop naming any node (rank 0): its owner is a collector above.
  Then `Error < Exit < Stopped c`, stops ordered by cause, ties to the earlier
  stage. An escape out of a stage is a top-level effect in flight, so the
  collector tears the siblings down as it files it and consumes the stops that
  teardown raises: `!{ exit 3 } | !{ /bin/sleep 100 }` is `exit 3` at once. The
  teardown's cause is the collector's own closing observation, the verdict only
  when no stage halted.
- **The closed-stdout stop.** A write to the host's stdout whose reader left
  raises `stop _ ReaderGone`, unnamed, minted at each write and striking
  nothing; it renders to nothing, status 141, as a SIGPIPE death does.
- **Panics are defects**, not signals: a panic unwinds to its machine's base.
  The top reports 101 after restoring the checkpoint; a collector tears the
  pipeline down with `Aborted`, no grace, and re-raises at the fold; a worker's
  is re-raised by `await`, `poll` and `race`, unwinding the observer too.
- **`poll h` is total.** A cancelled handle settles with the owner's record;
  `exit`, a foreign stop and a panic are re-raised, as `await` re-raises them.
- **`fail` is faithful.** `status`, `message`, `reason`, `cmd` and a `site`
  round-trip through `fail $e`; a status disagreeing with its reason's
  projection is refused, naming the field to fix.
- **`par` stays prelude code** whose fail-fast covers failures only; a stop, an
  `exit` or a panic passes its `try`, and the siblings run on. The language-level
  answer is a nursery, deferred.

## Which construct intercepts which signal

| construct | `fail e` | `exit n` | `stop ν c` |
|---|---|---|---|
| `try`, `?`, `attempt`, `retry`, `succeeds` | consumes | passes | passes |
| `audit B` | consumes into `` `err `` | passes, trail dropped | passes, trail dropped |
| `guard B C` | runs `C` shielded, then passes (or `C`'s halt) | same | same |
| `capture` (`!{…}`) | flushes, then passes; a flush failure pre-empts | same | same |
| `within`/`grant`/`unmask`, redirect | undo, pass | undo, pass | undo, pass |
| collector | folds | tears the siblings down, folds | naming its stage: consumes; foreign: tears the siblings down, folds |
| `await h`, `race` | re-raises | re-raises | own: the awaiter's error; foreign: re-raises |
| `poll h` | `` `err record `` | re-raises | own: `` `err record ``; foreign: re-raises |
| `par` (prelude) | cancels the siblings, re-raises | re-raises; the siblings run on | same as `exit` |
| the top of a run | `Ending::Raised` | `Ending::Exited` | `Ending::Stopped` |
| exarch tool call | error text | "exited" | rendered; `TimedOut` adds the timeout tip |

`exit` is consumed by nothing but the host. A stop in force at the step
before an `exit` wins: the poll fires first. A worker's `exit` or foreign stop
reaches the run that *observes* the worker and no further; an unobserved
worker's escape is data on its `` `done `` card.

## Laws

1. `try (return V) H = return V`; `try (fail e) H = H e`; `try (esc ℓ) H =
   esc ℓ`, the third covering every stop.
2. `M ? N = try M { |_| N }`; `attempt`, `retry`, `succeeds` inherit it.
3. `guard (return V) N ≃ shield (N ؛ return V)`; `guard (halt σ) N ≃ shield
   (N ؛ halt σ)`: the finaliser *and the re-halt* under the shield
   ([[decisions/261016_a-cleanup-is-shielded-within-its-grace|a-cleanup-is-shielded-within-its-grace]]).
4. Faithful `fail`: a record's `status`, `message`, `reason`, `cmd` and a
   `site` it has round-trip through `fail $e`; a `status` disagreeing with its
   `reason`'s projection is refused, naming the field to fix.
5. Pipeline verdict: a consumed stop contributes nothing, and so does a
   reader-gone stop naming any node (rank 0): its owner is a collector above,
   which will consume it, so no fold it passes ranks it over a halt that owner
   will not forgive. Then `Error 1 < Exit 2 < Stopped c (3 + c)`, the
   closed-stdout stop among the stops; two stops rank by cause; ties to the
   earlier stage. A refinement of the Agda's `_⊕_`, which keeps the earlier
   escape whatever its kind. The collector's own closing observation (the
   teardown's cause) is the verdict only when no stage halted: a base's
   closing poll outranks an error because the condition persists on its node
   and the next poll would raise it again, whereas a teardown's cause lives on
   the stages' nodes, which die with them, so it displaces nothing. That is
   what makes "contributes nothing" compositional: `!{ !{ fail [status: 7,
   message: x] } | !{ yes } } | head -1` is 7, the inner final stage's
   reader-gone stop naming the outer stage and ranking under the failure beside
   it; `!{ !{ guard { yes } { fail [status: 7, message: x] } } | cat } | head
   -1` is 7 likewise. An escape out of a stage is a top-level effect in flight,
   so the collector tears the siblings down as it files it and consumes the
   stops that teardown raises: `!{ exit 3 } | !{ /bin/sleep 100 }` is `exit 3`
   at once, wall or no wall.
6. Reader gone: a reader-gone stage's continuation past the dead write is
   discarded; its cleanup runs shielded; a cleanup that halts on anything but
   its reader-gone stop is forwarded (`guard { yes } { fail [status: 7,
   message: x] } | head -1` is 7); one that ends on its reader-gone stop is
   forgiven; a cleanup's write to the dead edge is stopped, not masked (an
   external's write to it is a blocked wait on a pipe the collector still
   holds, ended by the stop at the grace and forgiven). A corollary of 3 and
   9.
7. `code(fail (Raised n)) = n`; `code(fail (Process f)) = f.code()`;
   `code(fail (WorkerStopped o)) = o.cause().code()`; `code(exit n) = n`;
   `code(stop c) = c.code()`. `Ending::status` is this function
   (`Break::code`); the wire clamps it.
8. Panics are defects: a panic unwinds to its machine's base, where the top of
   a run reports status 101 after restoring the checkpoint, a stage's
   collector tears the pipeline down (`Aborted`, no grace) and re-raises at
   the fold, and a worker's is re-raised on `await`/`poll`/`race` unless the
   handle was cancelled first. A re-raised panic unwinds the *observing* run
   to its top, which restores that run's checkpoint; the `` `done `` card
   carries `` `panic `` as data for a host that reads it without observing.
9. A stop at a boundary is consumed iff it names that owner's node. The
   owners within the language are a worker (its own node) and a collector
   (its stages' nodes); every other node is the host's, so a stop naming one,
   or none, is a top-level effect that reaches the top uninterpreted. An owner
   consumes at most what one writer wrote on its node, so no stop has two
   owners.
10. Bound: a run that has been stopped ends within `grace + TEARDOWN_GRACE`
    of the strike, plus the step in progress, a cooperative blocking wait and
    a contended edge write aside. The closed-stdout stop strikes nothing, so
    it has no strike to bound from and no budget: it is re-minted at each
    write and read by no poll, a run that stops writing finishes at its own
    pace (as any Unix process that ignores SIGPIPE does), and a cleanup after
    it is bounded only by the wall, exactly as a cleanup after `return V` is.
    Its level-triggering is the kernel's: the pipe stays broken, so a
    cleanup's own write to it halts the cleanup, law 6's dead-edge write at
    the run's boundary. It prints nothing; the status is the whole report.

### Tests

| law | tests |
|---|---|
| 1 | `a_try_cannot_see_a_stop` (the witness: an unstruck stop, so a handler that ran would print), `an_audit_passes_a_stop`, `a_try_in_a_stage_cannot_see_its_reader_gone_stop`, `a_producers_try_cannot_change_its_reader_gone_stop`, `sigint_is_not_caught_by_try`, `a_closed_stdout_is_not_caught_by_try` |
| 2 | `a_fallback_passes_a_stop` |
| 3 | `a_cleanup_runs_shielded_and_the_stop_follows`, `a_cleanup_struck_late_gets_the_grace`, `a_cleanup_that_outlives_its_grace_is_stopped`, `an_abort_passes_a_cleanup`, `a_signalled_child_under_a_shield_is_a_failure`, `nested_cleanups_share_one_grace`, `successive_cleanups_share_one_grace`, the `cancel.rs` shield tests, `sigint_runs_a_guards_cleanup`, `sigint_bounds_a_cleanup_by_the_grace`, `a_timed_out_cleanup_returns_within_wall_plus_grace` |
| 4 | the `fail_*` tests in `builtins/misc.rs` |
| 5 | `breaks_join_by_rank_earlier_stage_breaking_ties`, `a_nested_reader_gone_stop_ranks_under_a_siblings_error`, `an_exit_in_a_stage_ends_the_pipeline_at_once`, `a_stage_halt_outranks_the_teardown_at_the_fold`, `a_nested_reader_gone_stages_cleanup_halt_is_the_verdict` |
| 6 | `a_reader_gone_writer_is_forgiven_by_its_name`, `a_foreign_reader_gone_is_forwarded`, `a_write_past_a_dead_edge_is_the_stages_reader_gone_stop`, the `a_reader_gone_stage*` pipeline tests, `a_cleanups_write_to_a_closed_stdout_halts_it` |
| 7 | `every_break_has_its_code`, the wire round-trip `stopped_round_trips` |
| 8 | `a_stage_panic_tears_the_group_down_and_is_reraised_at_fold`, `an_awaited_panic_is_reraised`, `a_polled_panic_is_reraised`, `a_raced_panic_is_reraised`, `a_cancelled_handle_polls_settled` |
| 9 | `a_join_names_the_outermost_node_attaining_it`, `a_stop_naming_the_stage_is_consumed_whatever_its_cause`, `a_stop_naming_the_workers_node_is_its_own_and_no_other_is`, `a_stage_inside_a_worker_names_the_worker`, `a_stronger_cause_lands_at_the_closing_poll`, `the_closing_poll_keeps_a_located_stop`, `a_lease_that_fires_during_the_last_step_is_still_the_awaiters_error`, `a_claimed_deadline_is_reported`, `a_teardown_is_the_verdict_when_every_stage_survived_it` |
| 10 | `a_cancel_all_waits_for_a_live_thread_stage_up_to_its_grace`, `an_abort_grants_no_grace`, `a_panic_mid_teardown_escalates_to_abort`, `a_cancelled_scope_is_read_between_slices`, `a_write_blocked_under_a_cause_is_stopped_within_a_slice`, `a_pump_blocked_on_a_full_edge_ends_within_a_slice`, `a_descendant_holding_the_pipe_does_not_hold_the_run`, `a_closed_stdout_is_a_stop` |

## Denotation

A sketch, to be stated over the Agda's existing semantics, not a replacement
for it. (The CK development cited here was retired on 2026-10-23; see `dev/agda/README.md`.) The signature is the kernel's extended by `fail e`, `exit n`, `stop ν
c` and `poll : 1 → L⊥`: `poll`, `write` and `exec` are input operations whose
response the world chooses, and `poll` has *no equations of its own*. It is
not a reader: `poll; poll ≠ poll`, the world advancing between two readings.
Its axiom is on worlds: the node store `W` grows monotonically (`W ≤ W′ ⇒
flag_W(ν) ≤ flag_W′(ν)`, `cancelled_at` set once), and a response is a
projection of `W` through the reader's own scope description ρ, the chain of
references in focus (`Shell::scope`) with its policy. Level-triggering is
`W`'s monotonicity, not a property of the responses, which drop to `⊥` on
leaving a cancelled child scope or entering a shield. Names ν are generated as
scopes are, a nominal set; concretely a node's identity (`ScopeId`).

A delimiter is a Σ-algebra homomorphic on every operation it does not
interpret: the shield transforms `poll`'s response (a tail-resumptive
forwarding handler), an owner interprets `stop ν c` selectively (a `catch`
with a predicate), and `guard` is a handler whose two clauses run under the
shield. The laws above are observational equivalences, `M ≃ N` iff `⟦M⟧W =
⟦N⟧W` for every admissible world, to be proved from the machine's reductions
as `Equivalence.agda` proves today's. Checkpoints are explicit: `ξpoll`, the
non-focal step that performs `poll` and halts on a nonzero response, fires
only at the machine's step boundaries (β, bind, apply, exec, the iterating
builtins, and once per slice inside a blocking operation); a handler clause's
entry is one step, and nothing fires mid-unwind, so no equation is broken by a
poll landing between a halt's consumption and its clause. Top-level effects,
`exit n` and the host's stops, are part of the denotation, not a failure of
it. What the extended world adds to its nondeterminism is exactly where a stop
lands, when a mask ends, and for a process which write is first dead.

## Why

- **`try` was a fiction over a stop.** A handler that caught a cancel ran
  until its first poll raised the same condition again, and the run's closing
  poll demoted only a successful settle, so a handler that settled without
  polling was caught at the top instead.
- **Forgiveness by shape was unsound.** The collector forgave a stage by the
  shape of its break, so a `try` inside a stage could turn its own reader-gone
  stop into a value. Forgiveness is now by name.
- **A cleanup under Ctrl-C could do nothing**: no command, no pipeline, no
  loop, since its first poll raised the stop again. The shield needs a stop
  that is an escape with a scope to mask.
- **Two contracts for one owner.** A worker a `cancel` detached reported
  "handle is cancelled", status 1, `` `raised ``; one whose lease expired was
  left attached and reported 124, catchable like any error. Both are now the
  owner's `WorkerStopped` record.
- **`fail $e` re-minted every record** as `` `raised `` with `cmd: fail`.

## Rejected shapes

- **A stop as a catchable error**, the previous code: `try` launders a cancel
  for one step.
- **Ownership by the stop's kind, or by cause equality.** Sound only while each
  kind has one owner on a chain; the name is one pointer.
- **Nearest node on a tie, with causes copied onto stage nodes.** A worker's
  lease `TimedOut`, copied onto a stage inside the worker, named the stage,
  and `await` raised an uncatchable stop instead of the lease error.
- **One cell for the owner's node and the current scope.** The terminal loan
  struck the shield, and a test act could strike nothing but the current
  scope. The node is run-invariant and the scope is not; they are two cells by
  the shell's own partition rule.
- **Wakes**: a sticky per-stage self-pipe shared by a stage and its pumps,
  re-armed by a blocking read. Two pollers hang; a re-arm racing a fire loses
  a wakeup.
- **A watch table and an expiry daemon.** Watches are one-shot, so after the
  first cancel nobody re-fired a shielded stage. Slices need neither.
- **A scope fixed in the byte endpoint at stage launch.** A stage's cleanup
  polled its shield but its blocked write read the stage node. The endpoint
  carries the stage's *name*; the scope a write reads is the writer's own,
  handed in.
- **A collector that writes nothing on its thread stages.** A panic or an
  anchor death put nothing on the chain, so a sibling ran on and the drain
  never returned.
- **The kernel as the sentinel**: closing the collector's read end so that
  `EPIPE`/SIGPIPE is the reader-gone stop for every writer. It hands the
  verdict to each tool's SIGPIPE disposition (a Rust-built child ignores it,
  prints `Broken pipe` and exits 1), and under a shield that death would be a
  process failure. The sentinel stays.
- **A strike for the closed-stdout stop.** Naming the final stage would make
  it forgivable; naming the run's node needs the node at every
  `write_stdout`. Its condition is the kernel's broken pipe, and its bound is
  declared instead.
- **A teardown decided by the heard key.** It bypassed the shield. The key
  strikes the node; the scope read at the next slice decides.
- **A pipeline that waits out a sibling's escape.** `!{ exit 3 } | !{
  /bin/sleep 100 }` ran the sleep to its end, and under a wall reported the
  wall, not the exit.
- **Delete the closing poll.** It reports a key whose stages all survived it;
  what was wrong was that it skipped errors.
- **A panic as a stage's error.** A ral program recovering from a ral bug is
  the one recovery that is never right.
- **`poll h` raising on a cancelled handle** while a lease-stopped one settles
  as data: two contracts for one owner.
- **`par` as a Rust builtin, or by a `guard` per `spawn`.** The first rebuilds
  a nursery in Rust; the second trips the recursion limit on large inputs.
- **A wall disarm "said in the comment"**: a daemon that had loaded `armed`
  could fire after the closing poll. `Deadline::disarm` is one `swap`, and a
  claimed firing is waited for on its own `fired` flag.

## Consequences

- Amends [[decisions/260905_the-cut-is-at-the-write|the-cut-is-at-the-write]]:
  the reader-gone break is `Break::stopped(ReaderGone, Some(stage))`, naming
  the writing stage's node. Amends
  [[decisions/260820_a-stage-ral-stopped-has-no-failure|a-stage-ral-stopped-has-no-failure]]'s
  forgiveness clause: forgiveness is by name, not by shape.
- SIGPIPE: ral ignores it in-process (Rust's runtime does so before `main`),
  so a closed stdout reaches ral's own write as `EPIPE`, which `write_sink`
  turns into the closed-stdout stop; a re-exec'd bundled tool and the
  pipeline anchor reset it to `SIG_DFL` themselves, and every child gets
  `SIG_DFL` from `reset_child_signals`. The hosts' `init_signal_dispositions`
  call and the frame writer's per-write suppression are gone.
- `try` never sees Ctrl-C, a wall or a teardown. The one stop a handler can
  meet is a worker's, converted into the awaiter's error.
- `par` no longer cancels its siblings on Ctrl-C or a wall: a knowing loss
  until a nursery exists.
- An `exit` in a worker reaches the run that observes it (`await`, `poll`,
  `race`) and no further; an unobserved worker's escape is data on its
  `` `done `` card.
- The Agda's obligations: `Esc` instantiated with `exit n | stop ν? c`, the
  world extended with the node store and a clock, a non-focal `ξpoll`, `βstop`
  consuming by name, and `_⊕_` refined to the cause order
  (`dev/agda/CK/RUST.md`). (The CK development cited here was retired on 2026-10-23; see `dev/agda/README.md`.)
- Windows has no sliced pipe wait: a thread stage's blocked I/O is
  interrupted (`CancelSynchronousIo`) when the collector writes its node and
  each pass while its chain carries a cause, and a pump each slice of
  `Pumps::join_under` once its scope does; an aborted call re-reads its
  scope. **Open:** an aborted `WriteFile` reports no count, so a write retried
  under a shield may repeat a prefix a live reader already took; writes are
  chunked (`PIPE_CHUNK`, 4 KiB), which narrows it. Exact counts need
  overlapped pipes, and the write end is shared with children.
- **The Agda**, after the Rust. Already proved: the sort split is one
  pattern match (`βtry-err`/`βtry-esc`); `guard` is one rule for both sorts
  (`βguard-halt`), the shield changing only what the cleanup's polls read;
  owner-frame masking for command handlers (`mask-layer`,
  `HandlerSelection.Removes`) is the shape of a `poll` owner;
  `CK.Machine.Focal` classes every scope-reading step as non-focal. To
  state: (1) `Esc` instantiated with `exit n | stop ν? c`, ν a generated name,
  absent for the closed-stdout stop, `_⊕_` extended to the cause order (law
  5); (2) the `World` extended with the node store `W` and a clock, a scope
  description ρ carried by the machine as `mask-layer`'s layer is, a non-focal
  `ξpoll`, the growth of `W` a labelled step, a blocked wait a repeated
  `ξpoll`; a stage and `spawn` as scoped operations (Wu–Schrijvers–Hinze 2014;
  Yang et al. 2022) whose inheritance rules differ: a stage runs under
  `extend(ρ, fresh)` and the collector's boundary, a worker under
  `extend(root, fresh)` and its own, inheriting nothing of ρ; an observation
  relation tying `Equivalence.agda`'s stack-indexed equivalence to the
  world-indexed one; (3) `βstop`: a boundary consumes `escaped ⟨ν, c⟩` iff ν
  is its own layer's name, and the pipe's `β∣R` family carrying the stage's
  name and running its cleanup before `report`, so law 6 is a theorem; (4)
  `shield!⟨since, grace⟩` as a `mask-layer` instance over a monotone clock,
  `CleanupPreempts` the base law 3 extends; (5) `fail`'s faithfulness as a
  lemma about the record projection. See `dev/agda/CK/RUST.md`. (The CK development cited here was retired on 2026-10-23; see `dev/agda/README.md`.)
- **A nursery**: a scoped `spawn` whose node is the call's, so a stop, an
  `exit` or a failure leaving the scope reaches its workers through the chain
  and the scope frame joins them; Trio's nursery is the precedent. Until then
  `par`'s fail-fast covers failures only, a knowing loss: before this change a
  stop was a catchable error, so `par`'s `try` did cancel the siblings on
  Ctrl-C or a wall.
- **Blocking host waits** outside the sliced rule: `Sink::Terminal` writes (a
  stopped tty), reads of an inherited terminal stdin, `ask`, file reads, a pump
  whose descendant holds the pipe in a group ral cannot address (`Joins`, or
  ral's own), an aborting pump's join on `RunningChild`'s panic path. Each
  blocks without reading its scope; law 10 excludes them.
- **A contended edge.** Two writers on one edge (a stage's own thread and a
  pumped child's stdout under `audit`) can both see `POLLOUT`; one blocks
  inside `write(2)` past the slice until the reader drains. The write end is
  shared with the child, so `O_NONBLOCK` is not ral's to set.
- **Nested runs.** A hook or an aside is dispatched under whatever scope its
  host passes to `run_under`, never under `Shell::scope`, so one dispatched
  from inside a cleanup is not shielded with it.
- **An external in a reader-gone stage's cleanup** that writes to the dead
  edge blocks on a pipe the collector still holds until the grace ends, then
  is terminated and forgiven; ral's own write is stopped at once. A write ral
  performs reads the edge; a process ral waits on is a cooperative blocked
  wait.
- **`Sink::pump` discards write failures**: a pumped child's output to a
  reader that left is lost silently, as before.
- **A deadline frame** (`within [deadline: …]`): an owner whose `R′` performs
  `fail`; its node would be the seventh minting point.
- **An external's death by SIGPIPE on the host's stdout** stays an ordinary
  process failure (`` `signaled 13 ``, 141), catchable: ral cannot tell it
  from a pipe of the command's own making.
- **A wipe's drain beyond the default grace**: `WORKER_DRAIN_GRACE` is sized
  to the default five seconds; a host granting a longer `cleanup_grace` would
  need the wipe to read it.

## Where

`Escape`, `Break::stopped`, `Break::of_child`, `final_poll`, `ThreadEnd` in
`core/src/types/flow.rs`; `Scope`, `ScopeNode`, `ScopeId`, `WorkerStop`, `POLL_SLICE`, `resolve` in
`core/src/process/cancel.rs`; `Shell::scope`, `Shell::poll`,
`Shell::write_sink` in `core/src/types/shell/mod.rs`; `Mooring::home` in
`core/src/types/mooring.rs`; the collector's `rank`, `recv`, `escalate`,
`cancel_all` and `StageObservation` in `core/src/runtime/pipeline/collect.rs`;
`convert_own_stop` in `core/src/builtins/concurrency/birth.rs`; `Ending` in
`core/src/run.rs`. See [[internals/cancellation|cancellation]] for the
runtime narrative and [[design/effects-handlers|effects-handlers]] for the
handler reading.
