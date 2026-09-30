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
C ::= F A  |  A → C  |  γ
```

A parameterised block has the value type `{A → C}`. `F A` carries only its
value type: there is no annotation on the returner, and the variables are
type and row variables.

## A command is `F Unit`

**A computation does two independent things: it writes bytes to stdout, and it
returns a value.** Stdout is an operating-system stream whose sink is chosen by
position — a redirect, a capture bracket, a [[design/pipelines|pipeline]]
stage's place in the line. The returned value is the evaluator's result. The
types say only the second; the first is said by which head a call has.

A *command* is a call whose head writes: an external, a builtin row with
`Output::Writes` applied at its arity, or a handler arm standing in for one. It
is a computation of type `F Unit` — it writes and returns nothing, like `cd`.
`{ echo hi }` is `{Command Unit}` and `{ 'hi' }` is `{Command String}`: the
checker tells a block that prints from one that returns because the program
does.

| program | type |
|---|---|
| `hostname` | `F Unit` |
| `echo hi` | `F Unit` |
| `return 5` | `F Int` |
| `from-bytes` | `F Bytes` |
| `from-json` | `F A` |
| `to-json $x` | `F Unit` |
| `audit { echo hi }` | `F Record` |

`audit` writes and keeps a value; it needs no special case. Every external
command is `F Unit` (`external_exec_comp_ty` in
`core/src/typecheck/infer.rs`), so `echo` and `^echo` show the checker one
shape. A head has one class (`HeadClass` in `core/src/typecheck/capture.rs`): a
binding, a value row that returns or writes, an arm standing in for a head, or
an external, in the lookup order the runtime shares.

## One coercion, `capture`, and who places it

**A `let` captures the command that produces its value; a function, block or
handle in that position binds what it returns, and `| from-line` turns what it
writes into a value.** So `let x = hostname` binds the text `hostname` writes,
and `let x = f` binds what `f` returns. The checker decides this once, from the
syntax of the right-hand side, before any type is inferred: `capture_sites`
(`⟦·⟧`, [[design/capture|capture]]) records each command whose output the `let`
wants, the `Exec` arm types it `F Unit` and answers `F String` for a recorded
node, and `annotate` wraps it:

```text
decode (capture M)
```

**`capture` is total and exact.** `capture M : F Bytes` for `M : F Unit` runs
`M` with its stdout captured and returns precisely the bytes `M` wrote —
nothing stripped, nothing decoded, and the body's own value ignored
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

## A stage writes

A pipeline stage feeds the next by writing, so every stage but the last is
`F Unit` and its head is no value row that returns (`Output::Returns`;
boundaries and `fold-lines` included). `length $xs | cat` is refused under
`Reason::PipelineStageWrites` (T0011) and asks whether `echo !{length …} | cat`
or `let x = length …` was meant; a value or block literal in stage position
"writes nothing to the pipe". The pipeline's value is its final stage's, always
([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).

## An arm stands in for what it is

`standsFor(c)` is the scheme of the base frame or arm already in force under
`c`, and `[String] → F Unit` for anything else (`Inferencer::stands_in`). An
arm for `curl` therefore writes and returns `()`; an arm for `detach` returns
what `detach` returns. A mismatch is `Reason::StandsIn`, with a sentence naming
what the arm stands in for and what to write. The catch-all `handler:` stands in
for every command in its block. The checker at the literal arm and the run-time
vet at install (`alias_arm_scheme`, `catch_all_stands_in`) apply the same check.

## Branching is plain HM over thunks

Every form that suspends a command — `if`, `case`, `try`, `?`, `guard`,
`within`, `grant` — takes a thunk `U C` and forces the one it chooses. A
literal `{ … }` is that thunk, and a thunk in hand (`$aaah`) is forced the same
way. An arm is a literal block, a lambda or a name; any other atom would be
hoisted and run before the form chose, and is refused at elaboration.

The arms of a join agree by unifying the *values* they return (`unify_arm`):
`T0010` and `T0020` for disagreeing values, `T0011` only for shape (`F` against
a function). A join of `()` against another type, where the `()` arm's tail is
a command, carries a hint: capture it (`ls | from-line`) or print in both. A
name that `let` bound to what a call returned, read at `()` where a word or an
interpolant is wanted, says what the call returns and what to write instead
(the `()` note).

## The calculus is ordinary CBPV

`F` is a functor from value types to computation types and the adjunction with
`U` is unchanged; nothing is annotated and nothing is graded. A sequence takes
its tail's type ([[related/cbpve|cbpve]]).

Three properties hold:

- a computation's type is stable under substitution and under abstraction;
- elaboration is total and type-preserving;
- capture is one coercion, placed by one syntactic walk, so the verdict and
  every printed type are independent of statement order.

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
[[decisions/260930_capture-is-decided-by-syntax|capture-is-decided-by-syntax]],
[[design/row-types|row-types]], [[invariants/fixed-arity|fixed-arity]],
[[related/rows-and-handlers|rows-and-handlers]] (the effect typing ral
declined). The volatile code map is [[map/core/typecheck|typecheck]].

**Realised in** [[internals/type-inference|type-inference]].

Cite: RATIONALE §"Values and commands", §"Pipelines follow their edges",
§"Structured values cross once"; `docs/SPEC.md` §17.
