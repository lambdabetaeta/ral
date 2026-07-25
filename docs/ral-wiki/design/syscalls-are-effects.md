# System calls are algebraic effects

**ral's foundational principle is that performing a system call — running an
external command — is performing an *algebraic effect operation*.** An external
name like `git` is an *operation*: it has a signature, it is *performed*, and it
yields a result or fails. Its *interpretation* — what actually happens — is the
operating system carrying out the syscall. The defining move is the separation
of the operation from its interpretation; everything dynamic in ral follows from
it.

This holds *period* — independent of handlers, of continuations, of any
reinterpretation machinery. [[design/cbpv|Call-by-push-value]] draws the line
between values and commands; this principle says what the command half *is*. The
two are the substrate and its reading.

## The operation namespace is the external-command boundary

- A name in head position resolving to neither a lexical binding, a builtin, nor
  a prelude name is an **operation**: an open name standing for an effect on the
  world ([[design/name-resolution|name-resolution]]).
- The language's own value-language — builtins, prelude, lexical `let` — is the
  *pure fragment*. It computes structured values and never crosses to the
  outside, so it performs no effect ([[design/builtins|builtins]]).
- **Not every kernel call is an effect.** A structured-query builtin (`list-dir`,
  `file-info`, `glob`) reaches the kernel yet is modelled as a pure structured
  primitive, *deliberately pulled to the value side* so it returns records rather
  than bytes ([[design/name-resolution|the syscall-bridge rule]]). What makes a
  name an operation is being an *open external name* whose meaning is supplied by
  its interpretation, not merely touching the OS. ral draws its effect boundary
  at the external-*process* surface — the `exec` / `wait` family that defines a
  shell.

## Operation, then interpretation

The algebraic reading is that the operation is *syntax* and its meaning is
supplied separately:

- Performing `git status` is a single forward step into the world that returns
  once — with a result, bytes, or a failure. The default interpretation is the OS
  performing the syscall; running an external command *is* invoking the effect.
- Because an effect happens only by performing an operation, the rest of the
  language is inert. `$name` dereferences a binding with no effect; a value is
  never a command. CBPV's value/command split is the syntactic guarantee that
  only a forced command in head position can touch the world ([[design/cbpv|cbpv]]).

## Handlers, and their continuations, are orthogonal

Separating an operation from its interpretation is what *makes reinterpretation
possible* — but reinterpretation is a layered choice along two independent axes,
neither of which the principle depends on.

- **Whether to add handlers.** ral can shadow an operation's default
  interpretation with a user block, `within [handlers: [git: H]] { … }`, for
  mocking, redirection, or instrumentation. This is additive: an external command
  is an effect whether or not any frame reinterprets it, and the ripples below
  flow from the effect identity, not from handlers.
- **Whether those handlers reify continuations.** A handler calculus *may* hand
  the handler a first-class continuation `resume k` — to drop, store, or invoke
  many times — or it may not. **ral's reify none.** They are the *tail-resumptive*
  fragment: a handler returns a value, that value is substituted for the
  operation, and the body resumes exactly once in tail position. This is
  deliberately *less* than algebraic-effect handlers usually offer, and faithful
  to the domain — an operation's result comes from the world and is consumed once,
  so *re-running the rest of the session with a different `git` output* is
  incoherent for an effect that has already perturbed the world. Multi-shot
  continuations earn their keep for *pure* effects (nondeterminism, backtracking),
  which ral does not model as external commands.

The handler fragment — deep, self-masking, tail-resumptive, no first-class
`resume` — is detailed in [[design/effects-handlers|effects-handlers]], realised
in [[internals/handler-dispatch|handler-dispatch]].

A precision for the formally minded: in the Plotkin–Power presentation an
operation already carries a continuation parameter, so "without continuations" is
informal — the exact claim is *without first-class, reified continuations*. The
continuation is the ordinary rest of the computation, never a value the program
captures.

## How it ripples

Reading the external boundary as the effect interface fixes the rest of the
dynamic design:

- **The value language is pure.** Effects happen only by performing an operation,
  so values, `$` dereference, builtins, and the prelude are inert — the source of
  ral's equational reasoning and safe `spawn` ([[design/cbpv|cbpv]]).
- **Authority is permission over effects.** `grant` attenuates *which* operations
  may be performed and on what arguments — a capability is permission over the
  effect set ([[design/grant|grant]]).
- **The capability gate sits at the effect-performance site.** The check lives
  where the operation is performed: the in-process gate for operations ral
  dispatches itself, the OS sandbox for operations a spawned child performs.
  `net` has no in-process gate because ral performs no network *operation* —
  there is no such effect in its signature, only the kernel's
  ([[design/two-enforcers|two-enforcers]]).
- **Dynamic scope is for authority.** An operation's ambient context and
  permitted set are inherited from the call site over a body's whole dynamic
  extent — a caller cannot lexically know which operations a callee will perform.
  Data stays lexical because it is not effectful ([[design/scoping|scoping]]).
- **Failure is an operation's exceptional outcome.** A command returns, emits
  bytes, or *fails* — failure is the effect's outcome, not a `Bool` value;
  `?` / `try` react to it ([[design/failure|failure]]).
- **Audit is a trace of operations.** The tree records operations, the scopes
  that framed them, and the capability checks at each, owned lexically by the
  scope ([[design/audit|audit]]).
- **exarch's sandbox bounds the effect set.** The agent evaluates each run under a
  grant over this namespace, bounding which operations the model may perform —
  the sandbox is authority over effects ([[design/exarch-architecture|exarch-architecture]]).

See also [[design/cbpv|cbpv]], [[design/name-resolution|name-resolution]],
[[design/grant|grant]], [[design/scoping|scoping]], [[design/failure|failure]],
[[design/effects-handlers|effects-handlers]],
[[related/handlers-of-algebraic-effects|handlers-of-algebraic-effects]] (the
operation/interpretation separation at its source).

Cite: README §"System calls are algebraic effects"; RATIONALE §"System calls are
algebraic effects"; `docs/SPEC.md` §0, §3, §3.2.
