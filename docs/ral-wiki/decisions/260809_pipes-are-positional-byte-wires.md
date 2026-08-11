---
status: active
supersedes: decisions/260601_modes-equality-constrained-shared, decisions/260603_ir-pipespec-annotation, decisions/260603_unconditional-mode-pass, decisions/260606_alias-head-defines-its-modes, decisions/260609_pure-pipe-equation, decisions/260610_value-edge-locality, decisions/260628_non-final-bytes-are-effects, decisions/260809_byte-only-pipelines
---

# Pipes are positional byte wires

> Amended the next day by `f5720bee` ("The route is the checker's business,
> and capture is the one coercion"): the bare `final_route: GroundRoute` this
> page describes does not reach the runtime. A pipeline instead carries an
> explicit `PipeYield` (`Last` or `Unit`), committed by the annotator so the
> evaluator reads what to report without knowing what a route is. The
> decision recorded here — one route per pipeline, no per-stage `Wire` — is
> unaffected; only the shape of that one route's runtime carrier changed.
> Read `final_route` below as `PipeYield`.

## Decision

**The surface `|` connects the left stage's stdout to the right stage's stdin;
neither endpoint must prove that it writes or reads.** Every interior edge is an
operating-system byte pipe allocated from stage position. A non-final stage's
returned value is discarded, while the final stage's payload route decides what
the pipeline as a whole reports.

The typing rule imposes no adjacency mode:

```text
Γ ⊢ M : F[ρ] A       Γ ⊢ N : F[σ] B
────────────────────────────────────
          Γ ⊢ M | N : F[σ] B
```

This is not a value pipe. A returned `Bytes`, `String`, or structured value is
never serialised onto an interior edge. A value-returning stage which writes
nothing gives its successor EOF; a value-returning stage which writes stdout
gives its successor those bytes. The value itself goes nowhere.

## Why no endpoint contract

A byte stream may be empty, and a process may ignore its input. Opaque external
commands make both facts unknowable statically, while the runtime already routes
descriptors from position. Rejecting either shape would turn a usage hint into a
false typing judgment.

The payload route remains distinct for a different reason: at a value boundary
it selects the evaluator's return or an implicit stdout capture, and at a
pipeline's final stage it selects what the pipeline reports as its own value. It
says nothing about whether a computation writes stdout and does not participate
in adjacency.

## The one surviving stage rule

A stage must have shape `F[ρ] A` — a computation ready to run, not a function
still waiting for an argument. Piping into an under-applied stage is a type
error that says to apply it rather than pipe into it:

```ral
echo hi | !{ |x| echo $x }
```

```text
[T0011] Error: two computations have incompatible shapes — one is a function, the other is not
 Help: a pipeline stage must be ready to run, not still waiting for an argument
 — apply it to its argument (`f $x`) rather than piping into it, or read the
 incoming bytes with a decoder such as `from-line` if it should consume the
 stream instead
```

This is the only static premise a stage carries about itself; nothing else
constrains it relative to its neighbours.

## Block literals in stage position: the footgun is admitted

A whole-stage block literal is **accepted**, not rejected:

```ral
cat f | { from-line }
```

typechecks cleanly. `{ from-line }` is a value — a thunk, `U(F[Bytes] Str)` —
and the stage rule above asks only that a stage be a *computation* ready to
run. A value satisfies that at `Return(Value, U C)`: the stage returns the
thunk itself as its (discarded, non-final) value. Nothing forces it, `cat`'s
bytes go unread, and `f` is never opened. This is a real footgun, and it is
**preferred to the alternative, deliberately.**

Two roads were open, and both were tried and abandoned:

- **A syntax-directed check** — reject a bare block literal in stage position —
  is not stable under the smallest meaning-preserving refactoring. `let stage =
  { from-line }` followed by `cat f | $stage` is the identical computation, one
  `let` away from the form the rule would have rejected. A rule that reads
  surface shape rather than meaning gives two programs with the same denotation
  two different verdicts, which is not a typing rule at all — it is a linter
  wearing one.
- **Rejecting on the type** — refuse whenever the stage's type is `A` with
  `A ≠ U C` for the pipeline's expected computation type — fails for a sharper
  reason. `A ≠ U C` is a *negative* premise: undecidable while `A` is still a
  variable, and not preserved by instantiation even once it resolves, since the
  same polymorphic stage can be applied at a use site where `A` later becomes
  `U C` and at one where it does not. No sound, decidable rule states it.

So the footgun stays, named rather than patched over. If it is ever
recovered, the right shape is a **non-blocking warning** — a lint that
observes "this stage looks unforced" without asserting anything about
acceptance, because acceptance is exactly the thing no rule here can decide.

## WF-2 is an obligation on operations, not an invariant assumed

The formation rule is simple to state and easy to get wrong in the
implementation: **`ρ = Bytes` implies the returned value is `Unit`.** A
byte-routed computation's value is discarded at capture (decoded from stdout
instead), so a non-`Unit` value under a byte route is a value the checker
promised exists and the runtime will never produce.

The rule cannot be assumed once and forgotten, because a route often starts as
a variable and is *grounded* to `Bytes` later, at whichever operation resolves
it — and every such operation must discharge the pairing at the moment it
grounds. What makes that undroppable is WF-2's own consequence: it leaves
exactly one byte-routed computation type, `F[Bytes] Unit`, named
`CompTy::bytes()` beside `CompTy::pure`. Landing on the byte side is
therefore *structural* — a unification with that computation whole, route and
value in one step — and no live code unifies a route against a detached
`Bytes` literal. Two operations land there:

- **`conclude_byte_side`**, the byte side of an arm join (`if`/`?`/`case`/`try`
  agreeing which side carries the payload): it unifies each arm with
  `CompTy::bytes()` whenever the join lands on `Bytes` — open arms included,
  the `Value Unit ⊑ Bytes` empty branch subsumed as it stands. This
  example exercises it — two arms disagree on where their payload lives, and
  the byte side's `Unit` obligation is what makes the disagreement a type
  error rather than a silent join:

  ```ral
  if true { echo hi } else { return 5 }
  ```

  ```text
  [T0011] Error: these two computations don't line up:
    payload route: one is captured from stdout, the other is a returned value
    return type: couldn't match Unit with Integer
  ```

- **`pin_arm_to_head`**, which unifies a handler or alias arm's route against
  the head it reinterprets. **This one did not enforce the pairing, and that
  was a live bug at HEAD.** It pinned the bare route and discarded the arm's
  value type in the same motion, so an arm whose own route was still an open
  variable at pin time — unconstrained by anything earlier in its body — could
  be pinned to `Bytes` under a byte-routed head while keeping a concrete,
  non-`Unit` value type the checker never re-examined. The result was silent
  type confusion: a release build exited `0`, printing `""` where the checker's
  own recorded type said `Int`. The repair keeps the arm's value type live
  through the pin and unifies it with `Ty::Unit` at the exact moment the route
  grounds `Bytes` — the same obligation `conclude_byte_side` already carried,
  moved to the grounding site that was missing it. A pin that now grounds
  `Bytes` against a non-`Unit` arm is `PinFailure::ByteHeadReturnsValue`, a
  reported `CompTyMismatch` naming the head and the arm's actual type, not a
  malformed program that runs anyway.

WF-2 is therefore not a single assertion at the type's definition — `PayloadRoute`
carries no such constraint structurally, and cannot: `Bytes` and a value type
are independent fields. It is carried instead by the shape of the byte side
itself: with `CompTy::bytes()` as the only way to spell it, a site that lands
a computation on bytes has no syntax for forgetting the `Unit` half.

## What was deleted

Computation types lose `input` and `output`; `PipeSpec`, `PipeMode`, `ByteMode`,
`ModeVar`, `Wire`, and the three-mode lattice they formed are gone in full. One
`PayloadRoute { Value, Bytes, Var }` remains, alongside its ground counterpart
`GroundRoute { Value, Bytes }`. The IR reflects the same collapse: a pipeline
carries one `final_route`, not a `Wire` per stage — there is no interior
adjacency left to annotate, because there is no interior adjacency rule left to
enforce.

## Relationship to the earlier rule

This decision supersedes
[[decisions/260809_byte-only-pipelines|byte-only-pipelines]] in full: neither
the producer's payload nor the consumer's input is pinned to `Bytes` at an
interior edge, and there is no second, rejected "value edge" — `x | f` is not a
special case, it is simply a pipeline whose stages happen to have `Value`
routes, exactly as legal as any other pipeline. What survives from the earlier
rule is its actual transport (byte-only operating-system pipes for every
interior edge) and its refusal of implicit value serialisation onto one.

Superseded with it, as consequences of the same collapse:

- [[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]]
  and [[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]],
  which built and then made mandatory the two-mode (`input`/`output`) lattice
  and its dedicated solver — there is one route now, not a pair, and it needs
  no second engine to disagree with.
- [[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]], whose
  per-stage `Wire` is the annotation this page's single `final_route` replaces.
- [[decisions/260606_alias-head-defines-its-modes|alias-head-defines-its-modes]],
  whose "fresh head defines its own modes, known head constrains the arm" rule
  is restated above as a route pin, not a mode pair pin — with the WF-2
  obligation it was missing.
- [[decisions/260609_pure-pipe-equation|pure-pipe-equation]] and
  [[decisions/260610_value-edge-locality|value-edge-locality]], which
  [[decisions/260809_byte-only-pipelines|byte-only-pipelines]] had already
  superseded and which now forward here with it. The first stated `x | f = f
  !{x}` at a value edge; the second located every judgment about such an edge at
  the site owning its facts. Neither has a subject left: there is no value edge,
  and the one edge fact that survives — this stage is not the last, so its
  stdout is a pipe — is positional, known to the allocator, and needs no
  judgment at all.
- [[decisions/260628_non-final-bytes-are-effects|non-final-bytes-are-effects]]:
  a value boundary binding the final computation of a sequence, and non-final
  bytes being effects rather than candidate values, was already independent of
  the *pipeline* adjacency rule — it concerned `let`/`Seq` boundaries, not `|`.
  It is superseded here only because its statement was in terms of `output`
  modes that no longer exist; the boundary rule itself — final computation
  wins, non-final writes are effects — is exactly this page's `ρ = Bytes ⇒
  Unit` value-boundary reading and needs no restatement beyond that.

See [[design/pipelines|pipelines]], [[design/capture|capture]], and
[[internals/pipeline-execution|pipeline execution]].
