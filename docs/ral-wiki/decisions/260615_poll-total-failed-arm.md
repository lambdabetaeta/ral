---
status: active
---

# A finished handle has one settle; the eliminators project it

**A finished concurrent block has a single observable *settle* — the bytes it
wrote plus its outcome, `` <ok: α | err> `` — and every eliminator projects that
one cached value. `await`/`race` unwrap it to the value (re-raising `err`); `poll`
reports the whole settle as data. Failure is data on the `poll` path and a raised
error on the blocking path — the same duality, one representation.**

The trigger was getting `poll`'s surface right. An earlier shape gave `poll` two
finished arms, `` `ready `` and `` `failed ``, the latter a lossy `{stdout, stderr,
status}` that dropped the structured error `await` re-raises. Pushing on it
revealed the cleaner factoring: the success/failure split is *orthogonal* to the
done/pending split, so it belongs inside the settle as a variant, not as sibling
arms — which is exactly [[invariants/optionality-via-variants|optionality as
variants]]. Reading `await` against the same lens then showed its top-level
`status` was dead weight (always `0`, since `await` only returns on success and
re-raises otherwise).

The settled surface:

- **One settle.** `Settle α = { stdout: Bytes, stderr: Bytes, outcome: <ok: α | err:
  ErrRecord> }`, where `ErrRecord` is the record `try` already hands a handler
  (`{status, cmd, message, line, col}`, reused — not re-invented). `status` lives
  inside `err`; there is no redundant top-level copy.
- **`poll h → F <pending: {stdout, stderr} | settled: Settle α>`** — non-blocking,
  *total*. Never re-raises a finished, failed, or panicked block; reports it as
  `` `settled `` data. A successful `poll` does not propagate the block's failure as
  its own. Still errors on a cancelled/forgotten handle (`ensure_live`) — detached is
  not finished. The `` `pending `` arm's `{stdout, stderr}` is the *partial* output
  written so far, added later in
  [[decisions/260702_partial-poll-pending-output|partial-poll-pending-output]]; see
  the note below on how that re-scopes this decision's consistency guarantee.
- **`await h → F {value: α, stdout, stderr}`** — blocks, unwraps `ok` to the value
  (re-raising `err`), so the happy path stays `let r = await $h; use $r[value]` and
  failure propagates by default. The dead `status` field is gone; a failed block's
  bytes are recovered with `poll $h` after a caught `await`.
- **`race`** projects the first finisher the same way; **`is-done`** is now total
  (`` `settled `` → true, `` `pending `` → false).

Why this is also a cleaner mechanism: the buffers are drained **once** at
completion into a cached `CompletedHandle { stdout, stderr, outcome }`, and
`await`/`race`/`poll` all project that one value. This removes the earlier
peek-vs-drain split — there is exactly one place bytes *leave* the buffers, and
repeated observations of a *settled* handle are consistent by construction.
[[decisions/260702_partial-poll-pending-output|Partial poll]] later re-scoped this:
the `` `pending `` arm *clones* the still-growing buffers (`peek_buffer`) rather than
draining them, so the drain-once invariant still holds (bytes leave exactly once, on
completion) but pending observations are no longer idempotent — they report
monotonically more output. The "repeated observations are consistent" guarantee is
therefore now precisely a guarantee about *settled* observations. A panicked worker (a
`Disconnected` result channel) settles as an `err` outcome carrying the same panic
error `await`'s blocking path reports, so `poll` reports it and `race` stops
spinning. `poll`'s `` `err `` payload is built through the shared
`evaluator::scope::error_record`, the same constructor `try`'s handler uses, so a
caught error and a polled failure are the same shape.

Why `await` keeps raising rather than also returning the settle: a blocking
"give me the result" verb that propagates failure by default is the ral idiom
([[design/failure|failure is status]]); making it total would force every call
site to `case` on the outcome. `poll` — the verb whose whole purpose is *not* to
commit — is where the outcome becomes data.

See also [[design/builtins|builtins]], [[map/core/builtins|map: builtins]],
[[invariants/optionality-via-variants|optionality-via-variants]],
[[design/failure|failure]]; `docs/SPEC.md` §13.3.
