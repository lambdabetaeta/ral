# The type system: Hindley–Milner with rows

ral is typed by Hindley–Milner inference with let-polymorphism, run over the
call-by-push-value [[map/core/ir|IR]] after [[map/core/elaboration|elaboration]].

**The two sorts of type mirror the [[design/cbpv|value/command]] split:**

- **value types** `A` describe inert data;
- **computation types** `C` describe effectful computations.

The value types are `Unit`, `Bytes`, `Bool`, `Int`, `Float`, `String`,
homogeneous lists `[A]` and maps `Map A`, closed and open records, the
thunk `{B}`, the opaque `Handle`, and type/row variables. Records are
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
`∀α ν. Integer → {ν α} → ν α`, "some producer of α". The variables are type,
computation, row and grade variables.

## A command is `F^w Unit`

**A command's value is its output.** A computation writes bytes to stdout, an
operating-system stream whose sink is chosen by position — a redirect, a
capture bracket, a [[design/pipelines|pipeline]] stage's place in the line — and
produces a result. The grade says how the result is produced, not whether
anything is written: it is deliberately a producer kind, not an effect, so
`{ echo pre; return 5 } : F^p Int`, and the bind rule stays total.

A *command* is a call whose head writes: an external, `^name`, a path head, a
builtin row that writes, or a handler arm standing in for one. It is `F^w Unit`.
`{ echo hi }` is `{Command}` and `{ 'hi' }` is `{Returns String}`; there is no
`Command A`.

| program | type |
|---|---|
| `hostname` | `Command` |
| `echo hi` | `Command` |
| `to-json $x` | `Command` |
| `warn hi` | `Returns Unit` |
| `return 5` | `Returns Integer` |
| `from-bytes` | `Returns Bytes` |
| `from-json` | `Returns α` (a boundary) |
| `audit { echo hi }` | `Returns Report(Unit)` |

The writing builtins are `echo`, `to-json`, `to-string`, `to-line`, `to-lines`,
`to-jsonl`, `to-csv`, `to-bytes`, `ints-to-bytes`, `help`, `explain`, `clear` and
`reset`. `warn` is `Returns Unit`: stderr is not the byte channel. `each`,
`fold`, `fold-lines`, `spawn`, `watch`, `service` and `audit` are polymorphic in
the grade of their body (`∀ε α. … U(F^ε α) …`), run it, stream its output and
keep its value; `fail`, `exit` and `diverges` are `F^ε α` for every ε. `map`,
`filter` and `sort-list-by` demand a value.

**Invariant.** `F^w A ⇒ A = Unit`. Every rule that introduces `w` does so at
`Unit`, every rule that copies a grade copies the value type with it, and every
builtin scheme pairs a grade variable with exactly one value variable; a sweep in
`core/tests/builtin_registry_property.rs` and a `debug_assert` in `generalize`
guard it. Grades are atomic — a grade variable binds only to `p` or `w` — so
there is no occurs check, no kind, and a grade is never weak.

Every external command is `F^w Unit` (`external_exec_comp_ty` in
`core/src/typecheck/infer.rs`). A head has one class (`HeadClass`, same file,
beside `exec_comp_ty`): a binding, a value row, an arm standing in for a head, or
an external, in the lookup order the runtime shares.

## One coercion, `cap`, and where it fires

**A command's value is its output: where a value is demanded, a command is
captured.** `cap : F^w Unit ⇝ F^p String` is the one run-time coercion, capture
then decode; `F^p Unit ⇝ F^w Unit` is a zero-cost upcast (a `()`-producer
regarded as a command, identity at run time), admitted at joins and handler arms
only. From `F^p Unit` there is no path to `F^p String`, so `let x = f` with `f`
returning `()` binds `()`; that keeps the two coherent. `cap` is inserted in
checking mode at a demand already resolved to `F^p`: the *bind* rule (the whole
right-hand side of a `let`), an *argument* to a value-demanding function, and a
*join* that settles on a value ([[design/capture|capture]]).

```text
M : F^p A   ⟹  M to x. N  binds x : A
M : F^w Unit ⟹  M to x. N  binds x : String, elaborated  cap M to d. decode d to x. N
M : F^ε̂ A   ⟹  ε̂ := p, then the first case
```

The bind rule's third case is the only defaulting in the system, and it is
local: a `let` demands a value. `annotate` wraps the recorded nodes.

**`capture` is total and exact.** `capture M : F Bytes` for `M : F^w Unit` runs
`M` with its stdout captured and returns precisely the bytes `M` wrote —
nothing stripped, nothing decoded
(`CompKind::Capture` in `core/src/ir.rs`, stepped by the `Frame::Capture` rules
in `core/src/evaluator/machine.rs`). Its one further clause is handler
semantics rather than decoding: bytes `M` wrote before failing are flushed to
the sink the capture replaced rather than lost.

**`decode : F Bytes → F String` owns everything lossy.** One trailing
terminator goes, and the rest must decode as strict UTF-8 or the step fails,
naming `| from-bytes` as the way to keep output that is not text. It is its own
node (`CompKind::Decode`), so every partial or lossy step from bytes to
`String` is syntax the operational semantics reads.

Both nodes are checker-inserted and have no surface syntax: a translation whose
meaning a session could redefine is not a translation
([[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]]). The composite
is close to the decoder tail `M | from-string`, which a user can write — and
that spelling remains theirs to write. A value produced by a decoder is
composed by application or bind, never by another pipeline edge — a `|` carries
bytes and nothing else.

## A stage feeds by writing

A pipeline stage feeds the next by writing, so every stage but the last is
accepted at `Unit` in any grade, and the pipeline's type is the last stage's.
A decoder (`from-*`, marked `BuiltinDiagnostic::Decoder`) in a non-final stage is
refused by its own mark (`stage_decoder`, `DecoderMidPipeline`, T0078):
"a decoder ends the byte pipeline: `from-json` returns a value and writes
nothing, so nothing reaches `cat`". A value or block literal in stage position
"writes nothing to the pipe" (`Reason::PipelineStageWrites`, T0011), and
`each { … } $xs | cat` is accepted. A stdout redirect *discharges* the grade,
`M > f : F^p A`; stdin and stderr redirects and `2>&1` preserve it. The pipeline's
value is its final stage's, always
([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).

## An arm stands in for what it is

`standsFor(c)` is the scheme of the base frame or arm already in force under
`c`, and `[String] → Command` for anything else (`Inferencer::stands_in`). An
arm for `curl` therefore has a command's value, its output; an arm for `detach`
returns what `detach` returns. A `()`-returning stub for an external is admitted
by the upcast and captures `""`; the installed scheme has the head's producer
kind, so a call through the arm types as the head does. A mismatch is
`Reason::StandsIn`, with a sentence naming what the arm stands in for and what
to write. The catch-all `handler:` stands in for every command in its block. The
checker at the literal arm and the run-time vet at install (`alias_arm_scheme`,
`catch_all_stands_in`) apply the same check.

## Branching is HM over thunks, joined by grade

Every form that suspends a command — `if`, `case`, `try`, `?`, `guard`,
`within`, `grant` — takes a thunk `U C` and forces the one it chooses. A
literal `{ … }` is that thunk, and a thunk in hand (`$aaah`) is forced the same
way. An arm is a literal block, a lambda or a name; any other atom would be
hoisted and run before the form chose, and is refused at elaboration.
`within`, `grant` and `guard` pass their body's `F^ε A` through; `audit` absorbs
it and returns `F^p Report(α)`.

**Joins take two passes.** Each arm is inferred against a fresh result, then
resolved: if some arm is `F^p A` with `A` not `Unit` (or unresolved), the target
is `F^p`, and every `F^w Unit` arm is captured; else if some arm is `F^w Unit`,
the target is `F^w Unit`, every `F^p Unit` arm upcasts and grade variables bind
to `w`; else the arms unify. So `if $c { hostname } else { echo b }` is one
`Command`; `if $c { echo $x } else { warn skip }` is a `Command` in any
position; `try { fetch } { |e| return 'none' } : F^p String`. A disagreement is
`T0010` and `T0020` for disagreeing values, `T0011` for shape or grade, with
hints derived from the two types: one arm is a command, whose value is its
output, while the other gives another type. A name that `let` bound to what a
call returned, read at `()` where a word or an interpolant is wanted, says what
the call returns and what to write instead (the `()` note).

## The calculus is CBPV, graded by producer kind

`F` is a functor from value types to computation types and the adjunction with
`U` is unchanged; the grade is an annotation on the returner, atomic and
unifiable like a type variable. A sequence takes its tail's type, and a discard
accepts any grade ([[related/cbpve|cbpve]]).

Three properties hold:

- a computation's type, grade included, is stable under substitution and under
  abstraction, so β and η preserve meaning (`let f = { hostname }; let x = f`
  binds the host name, like `let x = hostname`);
- elaboration is total and type-preserving;
- capture is placed by the type at three syntax-directed sites, and the result is
  principal on that stated rule. Joins are decided in program order with no
  pending constraints, so the verdict is not order-independent in one corner
  ([[decisions/260930_graded-f|graded-f]]).

Inference is annotation-free; generalisation happens at the `Bind` boundary. A
leaf, meanwhile, commits before inference begins: a bare word's value type is
fixed by the grammar that classifies it, never by the hole it lands in
([[invariants/numerals-denote-numbers|numerals-denote-numbers]]), so no
defaulting rule touches a value type. Soundness then rests on two independent
legs:

- **Recursion** is governed by the strongly-connected-component structure of
  binding groups: a non-recursive group generalises at its binding point, while a
  mutually recursive group (`LetRec` / `Rec`) stays monomorphic within the group
  and generalises only once its fixed point is reached.
- **No value restriction is needed.** Bindings are immutable, so there are no
  polymorphic references; and CBPV's `Bind` sequences a computation's effect
  *before* binding its result, so the thing generalised is always a value whose
  effect has already happened. What a program did not decide — data that
  enters through a decoder, or a container indexed by a computed key — has
  *weak* type variables, which a `let` never quantifies: one type per unit
  ([[decisions/260930_a-let-generalises-what-is-not-weak|a-let-generalises-what-is-not-weak]]).

A type error aborts with exit status 1 and a positioned expected-vs-inferred
message.

See also [[design/cbpv|cbpv]], [[design/capture|capture]], [[design/pipelines|pipelines]],
[[decisions/260930_graded-f|graded-f]],
[[design/row-types|row-types]], [[invariants/fixed-arity|fixed-arity]],
[[related/rows-and-handlers|rows-and-handlers]] (the effect typing ral
declined). The volatile code map is [[map/core/typecheck|typecheck]].

**Realised in** [[internals/type-inference|type-inference]].

Cite: RATIONALE §"Values and commands", §"Pipelines follow their edges",
§"Structured values cross once"; `docs/SPEC.md` §17.
