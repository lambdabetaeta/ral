# Algebraic effects and handlers

**Handlers are the orthogonal layer that reinterprets an operation.** That
[[design/syscalls-are-effects|system calls are algebraic effects]] is the
principle; whether to install a handler over an operation — and whether that
handler reifies a continuation — are two independent choices layered on top, and
ral takes the smallest on both: a restricted, continuation-free fragment.

ral installs effect handlers over a dynamic frame stack with
`within [handlers: [name: handler], handler: fallback] { body }`. A handler maps
an operation name to a lambda; the catch-all `handler:` applies when no per-name
entry matches. Handlers intercept **external commands only**, and only at the
*bare* head — lexical bindings, builtins, and the prelude are not shell aliases
and cannot be overridden, and `^name`, like a path head, skips the handler
stack too and reaches the external binary directly.

A per-name handler (and every alias) must be a **function of one parameter**,
such as `{ |args| … }`, applied to the intercepted command's argument list;
the catch-all must be a **function of two**, such as `{ |name args| … }`,
applied to the command's name and its argument list. The calling convention
is fixed by the surface position, and an arm's arity is its type, not its
spelling: `{ forward 'git' }` is an arm when `forward 'git'` takes the argv;
`{ echo hi }` is not, and is refused where it is written and again at
install.

**A handler reinterprets an operation; it cannot re-declare its type.** An arm
stands in for the head it is installed under: its type is that of the base
frame or arm already in force there, and `List String → Command` for any other
command. So an arm for `curl`, per-name or under the catch-all, has a command's
value, its output, and a `()`-returning stub is admitted and captures `""`; an
arm for `detach` returns what `detach` returns. The command
world is argv in, bytes out, uniformly. A value-returning, variable-arity
command is therefore not expressible; that capability belongs to the value
world instead, as a binding of fixed arity taking a list —
`let f = { |xs| … }; f [a, b, c]`. A name is a value or it is handled; if it is handled, it is a command, and a command's value is its output
([[decisions/260930_graded-f|graded-f]]).

ral's handlers are the **tail-resumptive fragment** of algebraic-effect handlers,
along three axes — the first two recorded in
[[decisions/260530_handlers-deep-self-masking|handlers-deep-self-masking]]:

- **Deep.** A handler persists across the dynamic extent of the body, re-wrapping
  each continuation, so successive operations all reach it.
- **Self-masking.** While a handler's own body runs, its frame is lifted from the
  stack, so a same-name call inside the body reaches the next outer handler (or
  the OS), never itself.
- **Tail-resumptive.** A handler receives the command name and arguments and
  either returns a value, forwards to the next enclosing frame, or fails. A
  returned value is *substituted for the intercepted operation* and the body
  resumes — that substitution **is** the resumption.

This is why there is **no first-class `resume`**: resumption is implicit,
exactly-once, and in tail position. What ral omits is the reified continuation
that non-tail or multi-shot resumption would require. The natural idiom is
wrap-and-forward, `within [handlers: [git: { |args| my-git ...$args }]] { ... }`,
which self-masking keeps from diverging; the live semantic question is therefore the
masking discipline — which handler is in scope at each point — not how a captured
continuation is re-entered.

## Owners and the shield are `poll` handlers

The same shape reaches past commands. Cancellation is one operation, `poll : 1
→ 1`, performed at every step boundary (`Shell::poll`) and once per slice
inside a blocked wait; under a nonzero join on the current scope it raises
`stop ν c`, an escape naming the node ν that attained the join. The machine's
stack already holds the frames that interpret it. `try` interprets `fail` and
nothing else, so a stop walks past it as `exit` does. A **base frame** — the
top of a run, a worker, a pipeline stage — is one handler: `{ return x ↦ R x;
fail e ↦ pass; stop ν c ↦ if ν = me then R′ c else pass; poll k ↦ if j ≠ 0
then stop ν j else k () }`, every other operation forwarded. Its return clause
opens with a *closing poll* (`final_poll`): the node is read once more after
the body settled, so a stop that landed after the last poll is not lost. A
worker's `stop` clause is invariant I6
([[decisions/261019_await-is-the-block-run-here|await-is-the-block-run-here]]):
it converts a `Cancelled` or `TimedOut` stop owned by any node but the session
root into the handle's error, and a root cause or an interrupt passes; a stage
forwards everything, and its collector consumes every stop naming that
stage's node; the top names no node of its own, so `exit` and the host's
stops are **top-level effects** no frame of the program interprets, read off
the empty stack as an `Ending`.

The shield is the fourth instance, `poll k ↦ k ()` while it masks:
`Frame::Cleanup` swaps `Shell::scope` for a shielded child for a `guard`
cleanup's extent and restores it on pop and abandon, exactly as
`Frame::Capture` swaps `io.stdout`. So `Shell::scope` is a dynamically scoped
cell beside `io.stdout`, *read* by every poll; the owner's name is
`Mooring::home`, run-invariant and the target of every *write*; the types say so, `Scope` against `ScopeNode`. Owner-frame
masking here is the same shape as self-masking above: a frame lifts what an
outer one would answer for the extent of its own body
([[decisions/261016_delimiters-are-handlers-for-poll|delimiters-are-handlers-for-poll]],
[[decisions/261016_a-cleanup-is-shielded-within-its-grace|a-cleanup-is-shielded-within-its-grace]],
[[internals/cancellation|cancellation]]).

Self-masking removes exactly the owning frame; nearer unmatched frames remain
visible too. In particular, an outer named arm wins over a nearer catch-all,
and that catch-all remains available while the named arm runs.

See also [[design/syscalls-are-effects|syscalls-are-effects]], [[design/control-operators|control-operators]], [[design/scoping|scoping]], [[design/grant|grant]], [[related/system-c|system-c]] (self-masking as capability-set subtraction), [[related/handlers-of-algebraic-effects|handlers-of-algebraic-effects]] (the calculus this fragment restricts).

**Realised in** [[internals/handler-dispatch|handler-dispatch]].

Cite: RATIONALE §"Effect handlers reinterpret external names";
`docs/SPEC.md` §9.6.
