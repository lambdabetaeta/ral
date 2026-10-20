# `await` is the block run here

**For a handle `h` born by `spawn [T] B`, `await $h` is observationally `!$B`
run at the `await`: the same value or the same raise, and the bytes `B` wrote
to its stdout and its stderr written, in that order, to the awaiter's current
stdout and stderr sinks. Awaiting again replays the same bytes and the same
ending.** The sinks are the awaiter's: inside a capture `let x = await $h`
binds the output as `let x = { … }` would, under `2> f` the stderr goes to
`f`, and inside a worker the bytes land in that worker's buffers. The ending
is the block's: a value returned, an error raised as it stands, an `exit` or
a stop re-raised, a panic resumed. The type says the same thing — a handle
carries its block's grade, `await : Handle^ε α → F^ε α`, so `await` of a
command's block is a command.

**The residue is stated, not hidden.** The block's effects happened earlier,
concurrently, in the worker's own shell, and the equation does not hold for:

- *shell state*: `cd`, environment overrides and the status set inside `B`
  never reach the awaiter;
- *the interleaving of the two streams*: all of `B`'s stdout is replayed,
  then all of its stderr;
- *a block that spawns*: inline in a run its inner `spawn` would be a loose
  session worker, while as a worker it joins that child before it settles
  ([[invariants/a-worker-ends-with-its-children|a-worker-ends-with-its-children]]),
  so the awaiter sees the child's bytes and failure too;
- *a cancelled or timed-out block*, which ends `worker was cancelled` or
  `worker timed out`, an ending no inline run has;
- *`exit`*, which ends the run that awaits, possibly a later one than the run
  that spawned it.

`poll` is the one observation that returns the bytes as data instead, and it
never writes. Do not add a second data-valued eliminator, or an `await` that
returns a record: the eliminators are this equation and its one data
projection.

See [[decisions/261019_await-is-the-block-run-here|await-is-the-block-run-here]]
(I1, I2) and `docs/SPEC.md` §11.2.
