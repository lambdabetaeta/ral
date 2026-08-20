---
status: active
---

# A stage ral stopped has no failure

**Past a `|`, a stage lives exactly as long as its reader needs it: ral itself
kills a non-final stage once its reader stage is gone, and the only death a
pipeline forgives is that kill.** No interior edge can deliver a broken-pipe
signal or a write error to the stage that writes it, so a producer's own
disposition toward that signal never again decides its pipeline's verdict.

## Decision

- ral holds a duplicate of each interior edge's read end until that edge's
  writer stage is reaped, closing only its own write-end copy as each stage
  finishes. EOF is a fact about the write end, so a reader's ordinary EOF
  discipline is untouched; SIGPIPE and EPIPE are facts about the read end, so
  with a second read end always open, neither can ever reach the stage that
  writes into that edge. A non-final stage has no channel left through which
  to observe its reader's death.
- Collection observes stages as they end, in whatever order that happens;
  once a writer stage's reader is reaped, ral sends the writer `SIGKILL` on
  Unix, a distinctive-code `TerminateProcess` on Windows, so the cascade runs
  tail-ward. That kill is the pipeline's one forgiven death — a non-final
  stage ral itself ended keeps no failure, because the rest of its output was
  owed to nobody. A stage that stops, wherever it sits, parks the whole
  pipeline at once rather than wedging a collector blocked on its neighbour.
- The kill is `SIGKILL`, not a caught signal, because any catchable signal
  reopens negotiation with the producer's handler table: a handler that
  ignores, delays, or reinterprets it hands the disposition question straight
  back to the producer's own code, which is exactly the dependence this
  decision removes.
- An exit status, once recorded, is never overwritten. The kill is sent only
  before the stage is waited, so it lands on a live process or a zombie —
  never a recycled pid, since an unreaped pid cannot be reused — and a
  zombie's recorded status is untouchable. The dangerous direction (a real
  failure silently forgiven) is therefore structurally impossible, not merely
  unlikely: forgiveness reads the wait status, and no kill can rewrite one.
- The held-open read end is what makes the kill exact rather than a race
  against the producer's own exit. Without it, a producer blocked in
  `write(2)` is woken by `EPIPE` the moment its reader's read end closes — the
  same event that makes the reader reapable — while ral still needs a
  scheduler wake to notice the reader is gone and act. A producer that checks
  its own write errors beats the kill through that window, and the
  disposition-dependence this decision removes returns at microsecond scale.
  Holding the read end removes the event the producer would have raced on,
  rather than trying to win the race.
- The residual nondeterminism is honest: a producer that would exit on its
  own account inside the kill's few-microsecond latency races the kill, and
  both outcomes are correct. The race selects *which event occurs* — the
  producer's own exit or ral's kill — never *how a recorded status is
  judged*. A status that was recorded, whichever event produced it, is kept.
- Cancellation outranks forgiveness: a death attributed to a cancellation
  already in force (Ctrl-C teardown) is kept even when the signal ral sent
  was `SIGKILL`. The kill this decision forgives is the collector's own,
  raised for exactly one reason — the reader is gone — and a `SIGKILL` raised
  for a different, already-recorded reason is not that kill.
- A parked pipeline (`SIGTSTP`) abandons its held read ends along with its
  stage handles and reverts to raw OS pipe behaviour; its verdict was already
  only its leader's exit, so no forgiveness question arises for a job that
  never finishes collecting.

## Rejected shapes

The causal SIGPIPE rule
([[decisions/260816_a-producer-that-outlived-its-reader|a-producer-that-outlived-its-reader]]):
forgive exactly the death SIGPIPE itself caused, and report anything else the
producer manages to exit with. Measured on Unix: `yes | head -1` succeeds,
but `python3 -c "for i in range(10**7): print(0)" | head -1` fails with
Python's own exit status, because Python sets `SIGPIPE` to `SIG_IGN`, takes
`EPIPE` from the write call instead, and exits on its own account — the same
cause, the opposite verdict, decided by a disposition the producer's author
chose for reasons that have nothing to do with the pipeline it happens to sit
in. Rust's `std` ignores the signal by the same default, so a plain
`println!` past a dead reader joins the same class. A rule that reads the
verdict off somebody else's program's signal handling is not reading a fact
about the pipeline.

The Unix exit-order clock, already recorded as rejected in
[[decisions/260816_a-producer-that-outlived-its-reader|a-producer-that-outlived-its-reader]]:
giving Unix the same reader-then-producer clock Windows used would have
subsumed the causal test, but Unix records no exit instant, and a two-thread
harness stamping the reaping wake time got the order wrong on a double-digit
percentage of runs in both directions — including turning a genuine failure
(`sh -c 'exit 1' | head -1`) into a silent success. A probabilistic verdict is
not a verdict, and this decision does not revive the clock on either
platform: it removes the event the clock was trying to read, rather than
reading it more precisely.

A catchable stop signal (`SIGTERM`, or a signal the producer could install a
handler for): rejected because it reopens exactly the negotiation the whole
decision exists to close. A caught signal's exit status is chosen by the
producer's handler, which returns the disposition-dependence problem in a new
shape — the handler's choices, not the write-error path this decision already
closed, but the same shape of problem.

## Consequences

- `!{ yes ; exit 5 } | head -1` goes from exiting 5 to exiting 0: the escape
  no longer occurs, because ral stops the stage mid-`yes`, before its `exit 5`
  ever runs.
- A producer loses the SIGPIPE-ignore opt-out bash affords it: a producer
  that must run to completion regardless of whether anything reads it can no
  longer rely on surviving a broken pipe. The rewrite is the one the
  stdin-feed refusal already teaches ([[design/pipelines|pipelines]]): run it
  as its own statement, or `spawn` it.
- Pipelines gain a liveness property they did not have under the causal rule:
  a producer that computes forever without writing no longer wedges a
  `… | head -0`-shaped pipeline, because the pipeline's extent is its final
  stage's extent, not its slowest producer's.
- The two platforms read one rule instead of two: Windows' exit-timestamp
  machinery — `ChildHandle::exited_at`, `GetProcessTimes`, the whole
  `outlived` computation — is deleted, and `docs/SPEC.md` §7.6 states one
  forgiveness rule true on both platforms rather than a causal rule on one
  and a clock-approximated rule on the other.
- Measured on Linux (a standalone C harness over `pipe2`/fork/exec, any-order
  `waitpid`, killing an unreaped writer on reaping its reader), 120 trials per
  case, once idle and once under load, 960 trials total, zero variance,
  2026-08-20: `yes | head -1` and the Python producer above are both forgiven
  120/120 in every run; `sh -c 'exit 1' | head -1` keeps its 1 in every run;
  and a producer that writes, sleeps 50ms past its reader's exit, then exits
  4, is forgiven 120/120 — the `exit 4` never occurs, the sleep dwarfing the
  kill's latency.

See [[design/pipelines|pipelines]], [[design/failure|failure]],
[[decisions/260816_a-producer-that-outlived-its-reader|a-producer-that-outlived-its-reader]]
and [[map/core/io-process|core IO/process]].
