---
status: active
generated_at_commit: ccb05833
---

# One delivery path

**A cancellation reaches a pipeline's processes exactly once, through the
collector.** ral had two deliveries — the cancel tree's and a signal relay's —
and a process cannot tell them apart, so it received the same interrupt twice.

## Decision

- **The relay is deleted.** `PipelineRelay`, `RELAY_PGIDS`,
  `relay_signal_to_groups`, and `relay_handler` are gone. The interactive
  SIGINT disposition is `interrupt_handler`, which calls
  `request_foreground_cancel(Interrupt)` and nothing else.
- **One send per process.** `CollectState::cancel_all(group, cause, delivered)`
  is ral's whole delivery: it cancels every live stage, then sends one grace
  signal — `kill(-pgid, sig)` and `SIGCONT` for a group it owns, per pid via
  `Watch::signal` for one it joined — waits `TEARDOWN_GRACE`, kills, drains.
- **A signal the kernel already delivered is not re-sent.** The tty gives a
  foreground pgid its own copy; the anchor swallows that copy and reports it,
  and `Event::Witnessed(cause)` carries `delivered: true`, so teardown cancels
  the thread stages and waits the grace but sends nothing. (Later
  `Event::Heard(signal)`, a key only to a terminal loan:
  [[decisions/260924_the-lent-terminal-returns-the-gesture|the-lent-terminal-returns-the-gesture]].)
- **`grace_signal(cause)` is the one cause→signal table**, read by
  `RunningChild::terminate` and `cancel_all` alike: `Interrupt` → SIGINT,
  `Explicit`/`Deadline`/`Terminate` → SIGTERM, `ReaderGone`/`RootAbort` →
  `None`, straight to the kill at every nesting level.
- **Attribution lives in `from_outcome`.** `CommandFailure::from_outcome(outcome,
  sent)` attributes the death to the cause `sent` names, so a pipeline external
  torn down under a deadline says so in the same words a standalone external
  does; `attribute_to` is private to `outcome.rs`.

## Rejected shapes

- **Keep the relay and de-duplicate by flag.** Two delivery paths that must
  agree, forever, about who already sent what — the flag is a second copy of
  the fact `delivered` already carries, kept in the wrong place.
- **Keep the relay for detached workers' pipelines only.** It contradicts the
  law that a detached worker is spared by Ctrl-C
  ([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-primitives-detached-vs-structured]]):
  the cancel tree spares it deliberately, and a relay reaching around the tree
  would kill it anyway.

## Consequences

- One Ctrl-C is one SIGINT per external, measured at two before. This left
  one gap, closed later: an external inside a thread stage heard the owner's
  group signal and its own waiter's by pid. Its waiter now holds a
  `Group::Joins` membership and opens only with a cause the stage scope does
  not hold, so it too hears one.
- `!{ yes | cat } | head -1` exits 0. The nested pipeline's reader-gone
  teardown no longer sends a catchable SIGTERM, so `yes` is not laundered into
  `yes: killed by signal 15`.
- A pipeline external under a deadline reports the deadline, not the signal.
- A `spawn`ed worker's pipeline survives a Ctrl-C at the prompt: the cancel
  tree spares the detached worker, and nothing relays around it any more.
  Windows kept two fan-outs past this decision — the console handler's
  Ctrl-Break to every group and `TerminateJobObject` of every job, and
  exarch's `relay_interrupt`, which spared only groups born detached — and
  they fell to the same rule later: the console handler raises the interrupt
  and nothing else, and a group hears Ctrl-Break only from the teardown of
  the scope that owns it.
- The anchor-witness race between the anchor's exec and its handler install is
  unchanged and benign: a signal in that window kills the anchor, whose own
  `Watch` reports `Event::Cancelled` and tears the pipeline down anyway.

See also [[internals/pipeline-execution|pipeline-execution]],
[[internals/cancellation|cancellation]],
[[decisions/260904_one-reaper-one-fold|one-reaper-one-fold]],
[[decisions/260905_the-cut-is-at-the-write|the-cut-is-at-the-write]],
[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-primitives-detached-vs-structured]];
map [[map/core/runtime|runtime]], [[map/core/io-process|io-process]].
