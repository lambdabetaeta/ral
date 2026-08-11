# The type system: Hindley–Milner with payload routes and rows

ral is typed by Hindley–Milner inference with let-polymorphism, run over the
call-by-push-value [[map/core/ir|IR]] after [[map/core/elaboration|elaboration]].

**The two sorts of type mirror the [[design/cbpv|value/command]] split:**

- **value types** `A` describe inert data;
- **computation types** `C` describe effectful computations.

The value types are `Unit`, `Bytes`, `Bool`, `Int`, `Float`, `String`,
homogeneous lists `[A]` and maps `[String:A]`, closed and open records, the
thunk `{B}`, the opaque `Handle`, and type/row variables. Records are
open-row-polymorphic; that fragment is its own page,
[[design/row-types|row-types]]. Records and maps share one runtime carrier but
answer different static questions — [[design/records-and-maps|records-and-maps]].

The computation types are three:

```text
C ::= F[ρ] A  |  A → C  |  γ
```

A parameterised block has the value type `{A → C}`.

## The payload route

**A computation does two independent things: it writes bytes to stdout, and it
returns a value.** Neither needs the type system. Stdout is an operating-system
stream whose sink is chosen by position — a redirect, a capture bracket, a
[[design/pipelines|pipeline]] stage's place in the line. The returned value is
simply the evaluator's result. What does need saying is which of the two a
*value boundary* observes when it demands the computation as a value. That is
the **payload route** `ρ`, and it is the whole of what a computation type
annotates:

```text
ρ ::= Value | Bytes | ρ-variable
```

- `Value` — the boundary takes the evaluator's return.
- `Bytes` — the boundary captures the computation's stdout, and reads those
  bytes as text (§"One coercion").

**The route is not an output predicate.** A `Value`-routed computation may write
any number of bytes; a `Bytes`-routed one may write none. It therefore cannot be
read as a promise about traffic, and nothing in the language asks it to be:
adjacency in a pipeline does not consult it, and neither does any runtime wiring
decision.

Five places read a route: a value boundary (`let`, `to`, a captured argument),
the join over a branch's arms, the final report of a process-staged pipeline, a
higher-order signature forwarding a thunk's route back out, and the pin that
installs a handler or alias arm under a head. A pipeline edge is not among
them.

| program | type |
|---|---|
| `hostname` | `F[Bytes] Unit` |
| `echo hi` | `F[Bytes] Unit` |
| `return 5` | `F[Value] Int` |
| `from-bytes` | `F[Value] Bytes` |
| `from-json` | `F[Value] A` |
| `to-json $x` | `F[Bytes] Unit` |
| `audit { echo hi }` | `F[Value] Record` |

`audit` writes bytes and keeps a value payload; it needs no special case in the
checker or the evaluator. Every external command is `F[Bytes] Unit`
(`external_exec_comp_ty` in `core/src/typecheck/infer.rs`), so `echo` and
`^echo` show the checker one shape.

**Display names the exceptional boundary behaviour in words.** A byte-routed
computation prints as `Command captured from stdout`; every other prints as
`Command A`. A stdout-captured command and a command returning a first-class
`Bytes` must not differ only by punctuation.

## WF-2 is carried by the one byte computation

The formation rule is one line:

- **WF-2** — `ρ = Bytes` implies the return type is `Unit`.

A byte-routed computation's returned value is discarded at capture, so a
non-`Unit` value under a byte route is a value the checker promised and the
runtime will never produce. The rule cannot be asserted once at the type's
definition: `PayloadRoute` and the value type are independent fields, and a
route is often a variable that some later operation *grounds*. What makes the
rule hold everywhere is its consequence: WF-2 leaves **exactly one**
byte-routed computation type, `F[Bytes] Unit`, named `CompTy::bytes()` (the
dual of `CompTy::pure`). Landing on the byte side of any decision therefore
means unifying with that computation *whole* — route and value in one
structural step — never writing a bare route:

- the byte side of an arm join (`conclude_byte_side`) unifies each
  non-subsumed arm with `CompTy::bytes()`;
- the alias/handler arm pin (`pin_arm_to_head`) demands the arm's value be
  `Unit` in the same breath as the pin that lands on bytes.

No live code unifies a route against a detached `Bytes`, so a new decision
site cannot forget the pairing — it has no way to spell half of it
([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).

## Where a route variable may live

A route variable is quantified in a `Scheme` like any other variable, and
`instantiate` refreshes it at each use. Two shapes are legitimate:

- **A declared slot** — the computation-typed argument of a builtin, and a
  scope's expected arm shape. Nothing consults it until a call site supplies a
  thunk.
- **A forwarded route** — a combinator that hands a supplied thunk's boundary
  behaviour back out, carrying the `(route, value)` pair together:
  `fold-lines :: ∀α ρ. U(α → String → F[ρ] α) → α → F[ρ] α`.

A builtin may **not** mint a route for its own result — one appearing nowhere
else in its signature. Nothing could ever ground it except an alias pin, and
forwarding half of a `(route, value)` pair is exactly the shape WF-2 cannot
police. `fail` is the one free route that is safe by construction: it never
returns, so no boundary observes it.

## One subsumption instance, at arm joins only

`F[Value] Unit` is also `F[Bytes] Unit`. The instance fires at the top of a
computation type only — it does not descend through `Thunk`, `Fun`, or rows —
and it is a judgment, not a unification step: `unify_route` demands equality on
ground routes. The join over the arms of `if`, `?`, `case`, and `try` applies
it:

- `Value A` beside `Value B` unifies `A` and `B`;
- `Bytes` beside `Bytes` stays `Bytes`;
- `Value Unit` beside `Bytes` coerces to the byte side, and the byte side ties
  every arm's value to `Unit`;
- `Value A` for non-`Unit` `A` beside `Bytes` is a type error, and the explicit
  spelling is `echo hi | from-string`;
- a wholly open join defers to the generalisation boundary that owns its
  variables, so an arm holding a recursive call is not forced to answer before
  it has one;
- divergence is neutral until another arm determines the route.

The join is decided by the arms' *types*, never by how an arm was written, so an
arm extracted into a `let` and forced back joins identically. `guard`, `within`,
and `grant` pass their body's route and value type through and need no arm rule.
A `case` obeys this at its arm *bodies*, which is where it was ever exercised:
its arms are a syntactic list, but an arm naming a handler is that handler
applied to the payload, and so joins and coerces as the written-out branch does
([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]).

A join whose informative arms are all still open is stored rather than decided
on the spot, and re-examined at the boundary that owns its variables — an inner
binding leaves an enclosing group's joins alone
([[decisions/260807_modes-solved-by-deferred-joins|modes-solved-by-deferred-joins]]).
There a residue equates rather than defaults, so route polymorphism survives.

## The route is inference machinery; only syntax survives it

Nothing downstream of the checker sees a route. Grounding one is the checker's
last act, and it spends the verdict immediately on syntax: a `capture` node at
a value boundary, and a `PipeYield` on a pipeline. The route types are private
to `typecheck`, so this is enforced by the module system rather than promised
by a convention ([[map/core/typecheck|typecheck]], [[map/core/ir|ir]]).

Of the two, only `capture` is a coercion — syntax inserted around a term,
changing its type. A pipeline's yield inserts nothing: it selects between two
readings of one form, `Last` reporting the final stage's value and `Unit`
reporting none. Elaboration makes that selection because the answer is
derivable — a pipeline's payload is its last stage's payload, and WF-2 forces a
byte-routed stage's value to be `Unit`, so a `Bytes` pipeline has nothing worth
shipping home.

## One coercion, `capture`, moves a byte payload to a value

The checker inserts a coercion where `M : F[Bytes] Unit` meets a value boundary
— at the right-hand side of a `let`, say. The precondition is a type, so no
runtime value test remains. What it inserts is a kernel node composed with a
library step:

```text
capture M to b. __decode-captured b
```

**`capture` is total and exact.** `capture M : F[Value] Bytes` runs `M` with its
stdout captured and returns precisely the bytes the handler collected — nothing
stripped, nothing decoded, nothing that can fail which `M` would not
(`CompKind::Capture` in `core/src/ir.rs`, evaluated by `eval_capture` in
`core/src/evaluator/comp.rs`). Its one further clause is handler semantics
rather than decoding, so it stays in the node: bytes `M` wrote before failing
are flushed to the nearest visible stream rather than lost
([[design/capture|capture]]).

**`__decode-captured : Bytes → F[Value] String` owns everything lossy.** One
trailing newline goes, and the rest must decode as strict UTF-8 or the step
fails, naming `| from-bytes` as the way to keep output that is not text. It is
an ordinary internal builtin beside the codecs
(`core/src/builtins/codecs.rs`), hidden from `help` and completion by its `_`
prefix, and it appears in the IR as an ordinary command — so every partial or
lossy step on the way from bytes to `String` is syntax the operational
semantics reads, not behaviour buried inside a node.

That split is the kernel/surface line drawn through one coercion: the byte
channel's handler is kernel, reading its output as text is surface. The
composite is close to the decoder tail `M | from-string`, which a user can
write. A value produced by a decoder is composed by application or bind, never
by another pipeline edge — a `|` carries bytes and nothing else.

## The calculus is ordinary CBPV plus one boundary annotation

`F` remains a functor from value types to computation types and the adjunction
with `U` is unchanged. The route is not a grade: it bounds no effect, licenses
nothing, and does not multiply along a bind — a sequence simply takes its tail's
route and value type. It is a tag on the returner naming which of two products a
value boundary reads ([[related/cbpve|cbpve]]).

Three properties hold:

- a computation's route is stable under substitution and under abstraction;
- elaboration is total and type-preserving;
- coherence follows from one subsumption instance and one coercion — the yield
  a pipeline carries is a choice of former, not a second coercion to reconcile.

Inference is annotation-free; generalisation happens at the `Bind` boundary. Its
soundness rests on two independent legs:

- **Recursion** is governed by the strongly-connected-component structure of
  binding groups: a non-recursive group generalises at its binding point, while a
  mutually recursive group (`LetRec` / `Rec`) stays monomorphic within the group
  and generalises only once its fixed point is reached.
- **No value restriction is needed.** Bindings are immutable, so there are no
  polymorphic references; and CBPV's `Bind` sequences a computation's effect
  *before* binding its result, so the thing generalised is always a value whose
  effect has already happened.

A type error aborts with exit status 1 and a positioned expected-vs-inferred
message.

See also [[design/cbpv|cbpv]], [[design/pipelines|pipelines]],
[[design/row-types|row-types]], [[invariants/fixed-arity|fixed-arity]],
[[related/rows-and-handlers|rows-and-handlers]] (the effect typing ral
declined). The volatile code map is [[map/core/typecheck|typecheck]].

**Realised in** [[internals/type-inference|type-inference]].

Cite: RATIONALE §"Values and commands", §"Pipelines follow their edges",
§"Structured values cross once"; `docs/SPEC.md` §20.
