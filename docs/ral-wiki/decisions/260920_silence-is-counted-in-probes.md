---
status: accepted
generated_at_commit: 8d9dee2b
---

# Silence is counted in probes, not in clock

**A peer is declared dead only for probes this process actually sent and the
peer did not answer. Elapsed time is never, by itself, evidence that anyone
is dead.**

## Why

`spawn_heartbeat` asked `last_seen.elapsed() >= deadline`. That reads as "the
peer has been silent for 25 seconds"; what it establishes is "25 seconds of
clock passed". Between the two sits an unwritten premise — that this process
was running to observe those seconds — and a suspended laptop falsifies it.
A Mac slept, woke, and its first heartbeat tick found a `last_seen` hours
old; synod showed *the assistant stopped, so this conversation cannot go on*
for an engine that was never asked anything.

The guest engine had the mirror of it: `last_frame.elapsed() >= patience.silence`
condemned the *front-end*, so a guest resuming beside its host killed the
in-flight run and exited `Corrupt`.

The bug is not the constant and not the clock source. `CLOCK_MONOTONIC`,
`CLOCK_BOOTTIME` and QPC disagree about suspension in three different ways
across the three hosts, and picking one only moves which host is wrong. The
premise is what fails.

## What holds instead

A probe is evidence because this process authored it. `Liveness::probes()`
turns the configured deadline into a count — `deadline / interval` — and the
heartbeat severs on that many unanswered `Ping`s. The engine counts `TICK`
polls that returned nothing instead of consulting a clock, and
`await_attached` spends patience in condvar waits it observed.

Suspension then needs no detection, no slack factor, no second clock. A
thread that is not scheduled sends no probes, so it accuses nobody; it wakes,
resumes asking, and the peer gets a full deadline to answer. A host too
loaded to run its heartbeat likewise has no standing to complain of silence.

`Severed::Silent(Duration)` keeps its payload and stops lying on its own: it
reported the configured patience, which was a falsehood only because the
judgment behind it was one. Counted in probes, the span it names is the span
that was watched.

## The rule this generalises to

**A timeout may bound how long we wait. It may never be the evidence that the
peer is dead.** Death is concluded from EOF — kernel-guaranteed, needing no
clock, which is why `WireTransport::new` runs no ticker at all — or from
probes that went unanswered. A duration bounding a test's patience or a
retry's spin is fine; a duration deciding that something died is a bug
waiting for a laptop lid.

Known boundary: the socket write deadline (`set_write_deadline`) is still a
span, since a blocked write cannot be counted in probes. A suspend inside a
stalled write can still read as `Closed`. Writes are short and rare, so this
is noted rather than fixed.

## See also

- [[design/engine-protocol|engine-protocol]] — liveness and severance
- [[map/core/engine-protocol|core/engine-protocol]]
