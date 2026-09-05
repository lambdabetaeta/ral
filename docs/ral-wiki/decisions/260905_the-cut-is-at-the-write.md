---
status: active
generated_at_commit: e536d55d
---

# The cut is at the write

**An interior edge is dead once its reader stage has ended, and a stage feels
that death at exactly one place — its next write to the dead edge — and
nowhere else.** Everything a stage does before that write is its own account
and runs to completion; nothing after it is promised. The forgiven death is
unchanged from [[decisions/260820_a-stage-ral-stopped-has-no-failure|a-stage-ral-stopped-has-no-failure]]
— only the moment of the cut moves, from the instant the reader is reaped to
the writer's own next write.

## Decision

- `crate::io::Edge` (`is_dead`/`mark_dead`) is one interior edge's fate: dead
  once its reader stage has ended, independent of whether anything has
  noticed yet.
- A ral-written stage's own write to a dead edge raises the break directly —
  `Sink::Pipe`'s `DeadEdge`, read back by `Shell::write_sink` as
  `Error::cancelled(ReaderGone)`. An external's write ral cannot see, so ral
  reads the edge in its place: once the reader is
  gone, `pipeline/sentinel.rs::listen` reads the `route::HeldEdge`'s read end
  it already holds, discards whatever bytes are pending (owed to a reader
  that left), and takes the first byte written after them as the kill's cue
  — SIGKILL on Unix, a distinctive-code `TerminateProcess` on Windows.
- That break, and only that break, is the pipeline's one forgiven death.
- Everything a stage does before its first dead write is its own account and
  runs to completion — its stderr, its files, its children that never write
  the edge, its own exit status if it never writes again. Nothing after that
  write is promised.
- A write blocked on a full pipe when the reader leaves is completed into
  ral's own read and then judged dead: a write not complete when the edge
  dies does not complete.
- A stage whose root redirect diverts every byte of stdout to a file never
  performs the write the cut watches for: `cmd > file | next` runs `cmd` to
  completion as a corollary of the rule, not as an exemption from it. The
  static exemption machinery — `feeds_pipe`, `resolve::diverts_stdout` — is
  deleted.
- Everything else from 260820 stands: the held read end, so no interior edge
  ever delivers a broken-pipe signal or a write error to the stage that
  writes it; SIGKILL/`TerminateProcess`, not a catchable signal; an exit
  status once recorded is never overwritten; a cancellation already in force
  outranks forgiveness; a producer that exits on its own account, without
  writing again, keeps that status.

## Rejected shapes

- **Keep the demand rule, document it.** The old rule cut a stage the instant
  its reader was reaped, wherever the stage's thread happened to be —
  before its first pipe write, even before its child was exec'd, whatever
  the scheduler chose. That is lazy IO with concurrency: effects performed as
  a side effect of demand, nondeterministically, and documenting it does not
  make the cut point a fact about the stage rather than the scheduler.
- **A hybrid — cut ral's own write, but kill a process the instant its reader
  leaves.** Two rules where one now suffices, and it leaves the motivating
  `sh -c 'echo world >&2; echo hello' | true` case exactly as racy as the old
  rule left it.
- **Close the held read end and let the process take `EPIPE`.** Reopens the
  disposition-dependent verdict 260820 removed: a producer's own signal
  handling, not the pipeline, would decide the verdict again.
- **Drain the dead edge forever and let the process finish.** `yes | true`
  never ends; a rule that buys exactness by giving up liveness is not the
  rule this decision is after.

## Consequences

- `yes | head -1` → 0; `!{ yes ; exit 5 } | head -1` → 0, cut mid-`yes`;
  `sh -c 'exit 3' | true` → 3, since it never writes and so is never cut;
  `sh -c 'echo world >&2; echo hello' | true` prints `world` and exits 0;
  `!{ echo x > f; echo hello } | !{ return () }` writes `f`.
- A stage that never writes again is never cut: `sleep 1000 | true` waits for
  the sleep, as in every shell — the stage was doing nothing for its reader,
  and its remaining work is its own. `cat | head -1` at an interactive
  terminal returns to the prompt only after the next typed line reaches
  `cat`, exactly as in bash and for the same reason. The liveness the old
  rule bought — a silent, never-writing producer ended by its reader's exit —
  is given back; it was bought with the laundering below.
- Measured on the old rule, 20 runs each: `sh -c 'echo world >&2; echo hello'
  | true` loses `world` 20/20; `!{ sh -c 'echo hello; echo x > f' } | true`
  never creates `f`; `!{ sleep 2; echo hi } | true` returns in 8 ms. Worse,
  verdicts were laundered rather than merely raced: `sh -c 'exit 3' | true`
  kept its 3 in 0/20 runs — a producer that never wrote anything was killed
  before it could exit, and its genuine failure forgiven. 260820's own
  `sh -c 'exit 1' | head -1` example kept its 1 only because `head` blocks
  until EOF and so never triggers the early cut.
- The old §7.6 redirect exemption argued that killing a stage mid-own-account
  work "would sever whatever the stage was still doing on the redirect's own
  account … and the forgiveness that follows would launder that loss into a
  silent success" — exactly what was happening to every other own-account
  effect. The exemption was a special case of a principle the general rule
  violated; it is now the general rule, and the exemption is gone as a
  clause because it follows from it.
- A write is the one operation that touches the edge, so it is the one place
  the reader's absence is a fact about the stage:
  [[design/syscalls-are-effects|write(edge) is an effect operation]], and "the
  reader is gone" is its interpretation there. For a ral-written stage the
  cut is exact; for a process it lands within the kill's few-microsecond
  latency after the first dead write — the same race 260820 accepted,
  confined now to after the write rather than open for the whole stage.

`pipes`, `conduit`, and iteratees replaced lazy IO's demand-driven effects
with a coroutine cut at the producer's own `yield`, deterministic because the
producer and consumer run synchronously on one stack. ral's stages are truly
concurrent — processes, not coroutines — so it cannot buy that determinism the
same way; it keeps the cut at the write instead and accepts a bounded race on
*which write* is first dead, never on *which non-write effect* occurs. This is
also SIGPIPE's own cut point, restored: what was wrong with SIGPIPE was never
where it cut — a producer's next write to a reader-less pipe — but that the
verdict it produced leaked the producer's own signal disposition
([[decisions/260816_a-producer-that-outlived-its-reader|a-producer-that-outlived-its-reader]],
[[decisions/260820_a-stage-ral-stopped-has-no-failure|a-stage-ral-stopped-has-no-failure]]).
ral keeps SIGPIPE's cut point and discards its verdict.

See also [[design/pipelines|pipelines]],
[[design/syscalls-are-effects|syscalls-are-effects]],
[[design/failure|failure]],
[[decisions/260816_a-producer-that-outlived-its-reader|a-producer-that-outlived-its-reader]],
[[decisions/260820_a-stage-ral-stopped-has-no-failure|a-stage-ral-stopped-has-no-failure]],
[[internals/pipeline-execution|pipeline-execution]],
[[map/core/runtime|runtime]]; `docs/SPEC.md` §7.6.
