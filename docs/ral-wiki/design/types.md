---
generated_at_commit: d56977ff
---

# The type system: Hindley–Milner with rows

ral is typed by Hindley–Milner inference with let-polymorphism, run over the
call-by-push-value [[map/core/ir|IR]] after [[map/core/elaboration|elaboration]].

**The two sorts of type mirror the [[design/cbpv|value/command]] split:**

- **value types** `A` describe inert data;
- **computation types** `C` describe effectful computations.

The value types are `Unit`, `Bytes`, `Bool`, `Int`, `Float`, `String`,
homogeneous lists `[A]` and maps `Map A`, closed and open records, their dual
the variant (a tagged sum over the same rows), the thunk `{B}`, the graded
handle `Handle^ε A` (what `spawn` returns; `await` gives back `F^ε A`), and
type/row variables. Records are
open-row-polymorphic; that fragment is its own page,
[[design/row-types|row-types]]. Records and maps share one runtime carrier but
answer different static questions — [[design/records-and-maps|records-and-maps]].

A type variable carries a *kind* — `number`, `comparable`, `scalar`, `sized` or
`data`, or none — so that `+`, `<`, `==`, interpolation and `length` state what they
take and a helper over them keeps its generality: `let log = { |n| echo "n: $n" }`
is `∀α:scalar`. See [[decisions/260930_operators-are-kinded|operators-are-kinded]].

The computation types are three:

```text
C ::= F^ε A  |  A → C  |  γ          ε ::= p | w | ε̂
```

A parameterised block has the value type `{A → C}`. `F` carries a *grade* ε,
a producer kind: `F^p A` produces a value `A` (printed `Returns A`), and
`F^w Unit` produces output and nothing else (printed `Command`). A grade
variable prints from the alphabet `ν ξ ο π`, so `explain retry` reads
`∀α ν. Integer → {Command[ν] α} → Command[ν] α`, "some producer of α". The variables are type,
computation, row and grade variables.

## A command is `F^w Unit`

**A command's value is its output.** A computation writes bytes to stdout, an
operating-system stream whose sink is chosen by position — a redirect, a
capture bracket, a [[design/pipelines|pipeline]] stage's place in the line — and
produces a result. The grade says how the result is produced, not whether
anything is written: it is deliberately a producer kind, not an effect, so
the body `echo pre; return 5` has type `F^ν Int` for a fresh ν, and the
bind rule stays total.

A *command* is a call whose head writes: an external, `^name`, a path head, a
builtin row that writes, or a handler arm standing in for one. It is `F^w Unit`.
`{ echo hi }` is `{Command}` and `{ 'hi' }` is `{Returns String}`; an unresolved grade prints `Command[ν] A`.

| program | type |
|---|---|
| `hostname` | `Command` |
| `echo hi` | `Command` |
| `to-json $x` | `Command` |
| `warn hi` | `∀ν. Command[ν] Unit` |
| `return 5` | `Returns Integer` |
| `from-bytes` | `Returns Bytes` |
| `from-json` | `Returns α` (a boundary) |
| `audit { echo hi }` | `Returns Report(Unit)` |

The writing builtins are `echo`, `to-json`, `to-string`, `to-line`, `to-lines`,
`to-jsonl`, `to-csv`, `to-bytes`, `ints-to-bytes`, `help`, `explain`, `clear` and
`reset`. `warn` has a fresh unit-result grade: stderr is not the byte channel. `each`,
`fold`, `fold-lines` and `audit` are polymorphic in the grade of their body
(`∀ε α. … U(F^ε α) …`), run it, stream its output and keep its value; `spawn`
and `watch` keep the grade in the handle they return, `Handle^ε α`, and
`await` gives it back as `F^ε α`; `fail`, `exit` and `diverges` are `F^ε α`
for every ε. `map` accepts either callback grade; `filter` and `sort-list-by` demand a value.

**Invariant, of inferred types.** `F^w A ⇒ A = Unit`. This is a theorem about
what inference produces, not a well-formedness condition on types: `F^w Int` is
a formable type, and a scheme can instantiate to it
(`∀ν α. U(F^ν α) → F^ν α` at `ν := w`, `α := Int`). It holds because every
rule that introduces `w` does so at `Unit`, every rule that copies a grade
copies the value type with it, and a grade variable is only ever unified
together with the one value type it is paired with, so output appears only at
`Unit` — given that every builtin scheme, and the error type of `fail` and
`try`, pair their grade variables in the same way. Before binding either grade,
`Unifier::unify_producers` (`core/src/typecheck/unify.rs`) refuses an output pair
against a type whose kind excludes `Unit`; returners and handles share this check.
`every_scheme_pairs_a_grade_with_one_value_type` in `core/src/shell/builtins/tests.rs`
checks builtin schemes; `rhs_bound_ty` in `core/src/typecheck/grade.rs` asserts
that a captured command's payload is `Unit`. Pairing is an inference invariant,
not a restriction on declarative type formation. Grades are atomic — a grade
variable binds only to `p` or `w` — so there is no occurs check, no kind, and a
grade is never weak.

Every external command is `F^w Unit` (`external_exec_comp_ty` in
`core/src/typecheck/infer.rs`). A head has one class (`HeadClass`, same file,
beside `exec_comp_ty`): a binding, a value row, an arm standing in for a head, or
an external, in the lookup order the runtime shares.

## One coercion, at `let`

**Only binding turns output into a value.** `cap : F^w Unit ⇝ F^p String`
is capture then decode; arguments, joins and handler arms use unification,
without wrappers ([[design/capture|grades and capture]]).

- A function right-hand side binds unrun as `return (thunk M)` (Let-fun).
- `F^w Unit` is captured and binds `String`; `F^p A` binds `A`.
- `F^ν A` fixes `ν := p`; a computation variable unifies with `F^p α`.
  This local rule is the sole grade default; no value type is defaulted.
- `infer_let` applies `rhs_bound_ty`'s decision to its inferred node,
  constructing the explicit capture/decode or Let-fun terms immediately.

Commands introduce `w`; builtin parameters demanding values and boundary
results introduce `p`. Other term producers and ordinary builtin results have
fresh grades, paired with their value types. A free unit producer can unify
with a command; a `let`-fixed `p` producer cannot.

`capture M` returns exactly the bytes its command body writes. `decode` drops
one trailing LF or CRLF and strictly decodes UTF-8; a failure names
`from-bytes` as the way to keep binary output. On a halt, capture drops its
buffer and writes nothing. Both terms have no surface syntax: a translation's
meaning cannot be redefined by the session
([[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]]).

`Command[ν] A` prints an unresolved producer, unless its paired type excludes
`Unit`, in which case it prints `Returns A`. `p` prints `Returns A`, and `w`
prints `Command`; only visible grade variables appear in a scheme's `∀`.
The complete rules and examples live in [[design/capture|grades and capture]].

## A stage feeds by writing

A pipeline stage feeds the next by writing, so every stage but the last is
accepted at `Unit` in any grade, and the pipeline's type is the last stage's.
A decoder (`from-*`, marked `BuiltinDiagnostic::Decoder`) in a non-final stage is
refused by its own mark (`stage_decoder`, `DecoderMidPipeline`, T0078):
"a decoder ends the byte pipeline: `from-json` returns a value and writes
nothing, so nothing reaches `cat`". A value or block literal in stage position
"writes nothing to the pipe" (`Reason::PipelineStageWrites`, T0011), and
`each { … } $xs | cat` is accepted. A stdout redirect gives a fresh result grade,
`M > f : F^ν A`; stdin and stderr redirects and `2>&1` preserve it. The pipeline's
value is its final stage's, always
([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).

## An arm stands in for what it is

`standsFor(c)` is the scheme of the base frame or arm already in force under
`c`, and `[String] → Command` for anything else (`Inferencer::arm`). An
arm for `curl` therefore has a command's value, its output; an arm for `detach`
returns what `detach` returns. A `()`-returning stub for an external is admitted
when its grade is free and captures `""`; the installed scheme has the head's producer
kind, so a call through the arm types as the head does. A mismatch is
`Reason::StandsIn`, with a sentence naming what the arm stands in for and what
to write. The catch-all `handler:` stands in for every command in its block. The
checker at the literal arm and the run-time vet at install (`alias_arm_scheme`,
`catch_all_stands_in`) apply the same rule (`Inferencer::arm`). Its parameters
are its slot's, and are read off its type, never its spelling (SPEC §17.7).

## Branching is HM over thunks, joined by grade

Every form that suspends a command — `if`, `case`, `try`, `?`, `guard`,
`within`, `grant` — takes a thunk `U C` and forces the one it chooses. A
literal `{ … }` is that thunk, and a thunk in hand (`$aaah`) is forced the same
way. An arm is a literal block, a lambda or a name; any other atom would be
hoisted and run before the form chose, and is refused at elaboration.
`within`, `grant` and `guard` pass their body's `F^ε A` through; `audit` absorbs
it and returns `F^ν Report(α)` at a fresh ν.

**Joins unify in source order.** Each arm is checked against its own fresh
result; the results then unify with the join as the expected side. No arm is
captured or reordered. A disagreement blames the later arm and says whether
it or an earlier arm is the command. `if $c { hostname } else { echo b }`
is one `Command`; `try { fetch } { |e| return 'none' }` is refused. Return an
explicit capture from the command arm to join it with text.

A fresh `return ()` can join either grade, unlike a unit producer whose grade
a `let` fixed to `p`. Diagnostics suggest explicit capture for text,
`succeeds { … }` for Bool, or decoding/returning a value of the required type.

## The calculus is CBPV, graded by producer kind

`F` is a functor from value types to computation types and the adjunction with
`U` is unchanged; the grade is an annotation on the returner, atomic and
unifiable like a type variable. A sequence takes its tail's type, and a discard
accepts any grade ([[related/cbpve|cbpve]]).

The capture rule is stable through abstraction and application:
`let f = { hostname }; let x = f` binds the host name, like
`let x = hostname`. Only a bind defaults a free grade; arguments and joins
impose ordinary equalities. Capture placement is local to the resolved
right-hand side ([[design/capture|grades and capture]]).

**Principality is relative to proper kinds.** Inference is principal against a
declarative system in which every generalised kind is *proper*: not empty, and
not a single nullary head. That is what the checker implements: it refuses an
empty meet, and a meet that is one nullary head binds the variable to that
head. A declarative system that allowed a variable of empty kind would type what
the checker rightly refuses: `let f = { |x| $[$x + 1]; length $x }` needs `x`
both `number` and `sized`, so it is refused though `f` is bound and never
called. The same properness is assumed of every scheme in the environment and
every builtin's.

Programs need no type annotations; generalisation happens at the `Bind` boundary. A
leaf, meanwhile, commits before inference begins: a bare word's value type is
fixed by the grammar that classifies it, never by the hole it lands in
([[invariants/numerals-denote-numbers|numerals-denote-numbers]]), so no
defaulting rule touches a leaf's value type. Soundness then rests on two independent
legs:

- **Recursion** is governed by the strongly-connected-component structure of
  binding groups: a non-recursive group generalises at its binding point, while a
  mutually recursive group (`Rec`) stays monomorphic within the group
  and generalises only once its fixed point is reached.
- **No value restriction is needed.** Bindings are immutable, so there are no
  polymorphic references; and CBPV's `Bind` sequences a computation's effect
  *before* binding its result, so the thing generalised is always a value whose
  effect has already happened. What an effectful right-hand side can still
  emit is typed independently of the variables the bind generalises: values
  pass through the scheme, bytes are untyped, and a failure carries an error
  of a closed data type. That last is required: a failure raised inside a
  generalised right-hand side escapes the bind, and preservation holds only
  because its payload mentions no generalised variable. What a program did not
  decide, data entering through a checked boundary, has *weak* type variables,
  which a `let` never quantifies: one type per unit
  ([[decisions/260930_a-let-generalises-what-is-not-weak|a-let-generalises-what-is-not-weak]]).

A type error aborts with exit status 1 and a positioned expected-vs-inferred
message.

See also [[design/cbpv|cbpv]], [[design/capture|grades and capture]],
[[design/pipelines|pipelines]],
[[design/row-types|row-types]], [[invariants/fixed-arity|fixed-arity]],
[[related/rows-and-handlers|rows-and-handlers]] (the effect typing ral
declined). The volatile code map is [[map/core/typecheck|typecheck]].

**Realised in** [[internals/type-inference|type-inference]].

Cite: RATIONALE §"Values and commands", §"Pipelines follow their edges",
§"Structured values cross once"; `docs/SPEC.md` §17.
