---
status: proposed
---

# Stateful handlers: a parameter threaded across interceptions

**A handler frame may carry a state value, given to each clause and replaced by
what the clause returns — a fold over the frame's operation stream.** This is
Plotkin–Pretnar's parameter-passing handler (§6.5) restricted to ral's
tail-resumptive fragment
([[related/handlers-of-algebraic-effects|handlers-of-algebraic-effects]]),
where it needs no continuation: the clause shape is
`(args, state) → (result, state′)`.

## The gap

A ral handler clause is stateless between interceptions: bindings are
immutable and the clause closes over a frozen environment, so nothing
observable can change from one interception to the next. Inexpressible today:

- **deterministic replay** — record a session's command outputs, then a
  handler that feeds them back one per interception (exarch agent tests);
- **quotas** — the first *N* `git` calls pass, then fail;
- **counting / instrumentation** without leaking through the filesystem or
  the environment.

## Proposed semantics

- A handler entry may carry an initial state alongside its block; the frame
  owns the state for its dynamic extent.
- On interception the clause receives the operation's arguments and the
  current state, and returns the substituted result and the next state —
  exactly once, in tail position. Resumption stays implicit; nothing is
  reified.
- Self-masking is untouched: the clause body runs with the frame stripped;
  only the state hand-off differs.
- The state is an ordinary `Value` — it serialises with the handler stack
  like any shell state crossing a re-exec ([[map/core/transport|transport]]).
- The state dies with its frame: `within` exit discards it. This is dynamic
  state on the dynamic context, consistent with [[design/scoping|scoping]].

Surface syntax is open (an entry shape like `[init: V, step: { |args, st| … }]`
versus a dedicated form); the semantics above is the proposal.

## Non-goals

No first-class `resume`, no multi-shot, no Benton–Kennedy return clause — the
[[decisions/260530_handlers-deep-self-masking|handler discipline]] is
unchanged. The clause remains a block; the only addition is the threaded
value.

**Verify:** a replay handler returns scripted outputs in order and fails when
the script is exhausted; a quota handler admits *N* then fails; state visibly
threads across ≥3 interceptions of one frame; the self-masking regression
suite is unchanged; state is discarded on frame exit.

See also [[design/effects-handlers|effects-handlers]],
[[internals/handler-dispatch|handler-dispatch]],
[[related/handlers-of-algebraic-effects|handlers-of-algebraic-effects]].
