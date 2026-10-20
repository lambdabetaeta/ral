# A worker ends with its children

**A worker's body ends, then each child still unclaimed is claimed and
awaited in birth order, exactly as `await` would
([[invariants/await-is-the-block-run-here|await-is-the-block-run-here]]):
their bytes land in the worker's own buffers, and the first failure among
them is the worker's, the rest being cancelled and drained within the cleanup
grace. A body that halts — a raise, an escape, a stop — cancels and drains
them too, and a body that panics cancels them. So no grandchild outlives its
parent worker by more than the grace.** The children are the worker's
*brood*: the handles of every `spawn` or `watch` born inside it, a pipeline
stage's included, in birth order. Claiming is atomic, so a child claimed
elsewhere first — an `await !{worker <id>}`, a `cancel` — is no longer the
brood's to settle, and of that claimer and the worker's own join exactly one
replays the child's bytes and meets its ending. The normal return waits rather
than cancels, so a block that starts workers has not finished until they have,
and `await` of it is still that block run here, its last implicit step
included. A run has no brood: its loose births belong to the session.

**Cancellation is hereditary.** A worker born inside a worker hangs under its
parent's node; one born in a run hangs under the session root; a child of
`par` or `all` hangs under the calling expression's current scope. So
`cancel $h` reaches everything `$h` started, and an interrupt or a wall on a
run reaches the fan-out of its own calls but never a session worker. A worker
converts a `Cancelled` or `TimedOut` stop owned by any node but the session
root into its own `worker was cancelled` / `worker timed out` error, so a
cancel made inside the session is a failure the awaiter can `try`, while a
stop the host owns passes as the escape it is.

See [[decisions/261019_await-is-the-block-run-here|await-is-the-block-run-here]]
(I4, I5, I6), [[internals/cancellation|cancellation]], and `docs/SPEC.md`
§11.1.
