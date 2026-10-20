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

A per-name handler (and every alias) must be a **unary lambda** `{ |args| … }`,
applied to the intercepted command's argument list; the catch-all must be a
**binary lambda** `{ |name args| … }`, applied to the command's name and its
argument list. The calling convention is fixed by the surface position, not the
value's runtime shape: a bare block `{ … }`, a non-lambda value, or a lambda of
the wrong arity is rejected at install time.

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
and that catch-all remains available while the named arm runs. The checked
frozen Agda `CK.Machine.HandlerSelection` development in `dev/agda` states this as ordered frame
deletion and proves preservation of predicates on the remaining code.
`HandlerCompleteness` connects command invocation to compositional semantics
when installed code belongs to the scoped fundamental fragment, including
recursion, capture, recovery and cleanup. Installation and capture preserve
that agreement; an example recovers from a handler's failure through capture.
This fragment includes direct external commands inside handler bodies but
excludes named calls, so general
forwarding and recursive handler invocation remain proof obligations. The
model also omits Rust's permanent base-handler layer between named run entries
and catch-alls.

The separate `CoveragePreservation` theorem now permits named calls inside
stored code and recursive handler bodies: renaming, substitution and semantic
evaluation preserve membership in the interpreted fragment. `ScopeCoverage`
shows that dispatch selects covered code and retains a covered scope after
masking. The result theorem needs no ambient-code premise because command
results in this core are `Unit`; the invocation completeness proof still
needs the scope premise. This supplies the invariant for general named
forwarding. Agreement for all finite executions of that fragment remains open.

The active CEK development now supplies a separate permanent base table in
`CEK.HandlerSelection`: explicit user names are searched first, base entries
second, and user catch-alls last. Its executable selector is total, sound,
complete and deterministic. Masking deletes exactly the chosen user frame,
retains all nearer unmatched frames in order and preserves the base table.
Base entries run unmasked. `CEK.RuntimeHandlers` evaluates user arms as typed
lexical closures, with argv alone for named arms and name then argv for
catch-alls; base entries are separate host tokens. `CEK.HandlerApplication`
builds typed CEK applications and returns unit/error/exit across the arm's and
caller's differing type scopes. Continuation and capture proofs cover the
return frame. Configurations now carry handler scope separately from lexical
bindings. Installation, named/catch-all invocation, exact owner masking,
restoration on return/error/exit and unmasked host base calls execute.
Base replies retain native unit/error/exit and bounded byte traffic; complete
error records survive scope crossing. Checked traces cover forwarding to base,
successive calls, bypass, catch-alls, capture and escaping closures using caller
handlers. Both capture simulations cover the new rules.

**The provisioned base table is permanent along every finite execution prefix.**
`CEK.HandlerInvariant` requires the same table in active and saved handler scopes;
internal computation, polymorphic continuations and capture routing preserve it.
`CEK.HandlerPreservation` covers every configuration step and establishes the
invariant for every state reachable from a closed program. Exact table equality
preserves all base lookups, without termination or world-determinism assumptions.
**Handler entry and restoration account for exactly two steps.**
`CEK.HandlerAgreement` relates finite runs of installation and selected
named/catch-all invocation to their body executions at an empty continuation.
The body has identical output, outcome and world effects; completion restores
the caller scope. Reverse extraction removes precisely entry and restoration,
providing a strict measure without assuming termination. Nested handlers,
captures and fallible world operations are covered. This is agreement between
machine runs.

`CEK.CommandSemantics.Call` unifies direct, named-arm, catch-all, native base and
external-fallback execution, independently of the machine. Arm evaluation is
an explicit parameter: arms run masked, and every completed call restores
caller handlers. `CEK.CommandAgreement` proves exact finite-run agreement for
both command heads when the parameter is interpreted by typed arm application
runs. Output, outcome and the complete final context are preserved. Refining
the arm relation refines calls.

`CEK.TermEvaluation` gives the independent arm interpretation, mutually with
term, closure, application and polymorphic-builder evaluation. Calls use the
same selection rules, while capture evaluates under the accepting stdout world
and uses real replies when flushing outward. Completion preserves the incoming
handler scope. Checked derivations include forwarding, repeated calls,
catch-alls and captured output. `CEK.TermSoundness` proves refinement to machine
arm runs at arbitrary caller and captured scopes, including enclosing stdout
captures. Output routing, cross-scope error conversion and the complete final
context are preserved. A continuation interpretation now supports backward
reflection of internal steps and caller-scope restoration.
`CEK.TermEffectsReflection` also recovers independent arm derivations from
actual dispatch and command replies. `CEK.TermCompleteness` gives exact
finite-run agreement at the empty continuation, preserving output, typed
result and the entire final context. Arms may be recursive and polymorphic;
worlds need be neither deterministic nor total. Contextual handler laws and
Rust refinement remain open.

See also [[design/syscalls-are-effects|syscalls-are-effects]], [[design/control-operators|control-operators]], [[design/scoping|scoping]], [[design/grant|grant]], [[related/system-c|system-c]] (self-masking as capability-set subtraction), [[related/handlers-of-algebraic-effects|handlers-of-algebraic-effects]] (the calculus this fragment restricts).

**Realised in** [[internals/handler-dispatch|handler-dispatch]].

Cite: RATIONALE §"Effect handlers reinterpret external names";
`docs/SPEC.md` §9.6.
