# Algebraic effects and handlers

**Handlers are the orthogonal layer that reinterprets an operation.** That
[[design/syscalls-are-effects|system calls are algebraic effects]] is the
principle; whether to install a handler over an operation — and whether that
handler reifies a continuation — are two independent choices layered on top, and
ral takes the smallest on both: a restricted, continuation-free fragment.

ral installs effect handlers over a dynamic frame stack with
`within [handlers: [name: handler], handler: fallback] { body }`. A handler maps
an operation name to a block; the catch-all `handler:` applies when no per-name
entry matches. Handlers intercept **external commands only** — lexical bindings,
builtins, and the prelude are not shell aliases and cannot be overridden.

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

See also [[design/syscalls-are-effects|syscalls-are-effects]], [[design/control-operators|control-operators]], [[design/scoping|scoping]], [[design/grant|grant]], [[related/system-c|system-c]] (self-masking as capability-set subtraction), [[related/handlers-of-algebraic-effects|handlers-of-algebraic-effects]] (the calculus this fragment restricts).

**Realised in** [[internals/handler-dispatch|handler-dispatch]].

Cite: RATIONALE §"Effect handlers: deep with self-masking"; `docs/SPEC.md` §3.2.
