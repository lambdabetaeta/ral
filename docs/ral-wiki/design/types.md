# The type system: Hindley–Milner with byte modes and rows

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

**Computation types carry a three-mode spec.** A computation has the type
`⟨i, o, r⟩ A`, where `A` is a value type: the computation returns a value of
type `A` under the spec. The spec is `PipeSpec` in `core/src/mode.rs`:

- `i` is the input mode: `Bytes` when the computation reads the byte channel, `∅` when it does not;
- `o` is the output mode: `Bytes` when the computation writes the byte channel, `∅` when it does not;
- `r` is the result mode: the conduit that carries the computation's payload.

Each mode is `∅`, `Bytes`, or a mode variable. The three modes share one
unification, generalisation, and display machinery. Adjacent
[[design/pipelines|pipeline]] stages connect through the producer's `result`
and the consumer's `input`; every interior connection is `Bytes` on both sides.
The final stage keeps its own result mode, so a pipeline may return a value at
its boundary. A parameterised block has the type `{A → B}`.

**The result mode locates the payload.** `result = Bytes` means: the
computation's payload is its byte channel. `result = ∅` means: the payload is
its return value. `output` is independent chatter, so it does not constrain
`result`; the one remaining well-formedness condition is:

- **WF-2** — `result = Bytes` implies that the return type is `Unit`.

WF-2 is the mode solver's to keep
(`core/src/typecheck/mode_solver.rs`), checked per arm wherever a join lands on
the byte side. WF-2 is enforced there rather than merely asserted: the byte
side ties every arm's value to `Unit`, arms whose conduit was still open
included. `core/src/typecheck/builtins.rs` keeps its own assertions, which
guard hand-written signature tables at construction rather than any join. A
computation therefore has one payload: its return value or its byte channel,
never both. For example:

- `echo hi : ⟨∅, Bytes, Bytes⟩ Unit` — the bytes are the payload;
- `return 5 : ⟨∅, ∅, ∅⟩ Int` — the return value is the payload;
- `audit { echo hi } : ⟨Bytes, Bytes, ∅⟩ Record` — the computation writes bytes, and the record is the payload;
- `from-json : ⟨Bytes, ∅, ∅⟩ A` — a decoder reads bytes and returns a value.

The third example shows that a computation can write bytes and keep a value
payload. `audit` needs no special case in the checker or in the evaluator. An
external command has the type `⟨i, Bytes, Bytes⟩ Unit` with a fresh input mode
`i` (`external_exec_comp_ty` in `core/src/typecheck/infer.rs`), so `echo` and
`^echo` show the checker one shape.

**The result mode is decided at every source-tree node.** An introduction
rule sets it; propagation copies it; a join computes it; a shape-forcing
expectation grounds it. A payload decision against an unresolved result mode
pins the mode to `∅`. One decision is a deferral: a join computes its mode
from the arms that carry information, and a `∅`-at-`Unit` arm carries none —
it is the join's identity, compatible with either side by the subsumption
instance below. A join whose only informative arms are still open — say a
recursive call, whose mode is its own function's — therefore stays open, and
grounds at the binding group's fixed point or at the first payload decision.
Result-mode variables otherwise appear only in declared signature slots:

- the computation-typed argument of a builtin, for example `spawn`, `each`, `map`, and `fold`;
- the expected arm shape of a scope.

A slot variable is quantified like every other mode variable, for example
`spawn : ∀ i o r β. U (⟨i, o, r⟩ β) → Handle β`. No elaboration decision reads
a slot variable.

**One subsumption rule has one instance.** The type `⟨i, o, ∅⟩ Unit` is also
the type `⟨i, o, Bytes⟩ Unit`. The rule applies at the top of a computation
type only:

- it does not descend through `Thunk`, `Fun`, or rows;
- it is not a unification rule — `unify_mode` demands equality on ground modes.

The join over the arms of `if`, `?`, `case`, and `try` applies the instance:

- a join with a byte-payload arm lands wholly on the byte side;
- a `∅`-at-`Unit` arm subsumes into the byte side;
- a byte-payload arm beside a `∅`-non-`Unit` arm is a type error, and the explicit spelling is `echo hi | from-string`.

`guard`, `within`, and `grant` pass their body's type through and need no arm
rule.

**Three relations, not one.** Equality is only one of them, and the other two
are solved rather than folded
([[decisions/260807_modes-solved-by-deferred-joins|modes-solved-by-deferred-joins]]).
Pipeline adjacency is equality: ground `∅` never meets ground `Bytes`. A
compound form's channel end is the *join* `⊔` of its parts' ends — `∅` the
identity, `Bytes` absorbing — constraining the compound's end alone and never
writing back into a part's. A branch's or scope's input end *alternates*
instead, because only one arm runs, so arms that disagree on stdin leave the
input unknown for a downstream stage to pin rather than clashing. The arm
result is the third: which conduit carries the payload, decided under the one
subsumption instance above.

A join whose informative arms are all still open is not decided on the spot;
it is stored and re-examined at the generalisation boundary that owns its
variables — an inner binding leaves an enclosing group's joins alone — and
there a residue equates rather than defaults. So an arm holding a recursive
call — whose mode is its own function's, still under inference — no longer
forces an answer before it has one, and mode polymorphism survives
(`∅ ⊔ μ = μ`).

**One coercion, `capture`, moves a byte payload to a value.** The checker
inserts `capture M : ⟨i, ∅, ∅⟩ String` where `M : ⟨i, o, Bytes⟩ Unit`, for
example at the right-hand side of a `let`: the capture swallows `M`'s byte
output into the result, so only the stdin demand rides through. The precondition is
`result = Bytes`, which is a type, so no runtime value test remains. `capture`
is an IR node (`CompKind::Capture` in `core/src/ir.rs`) with one evaluation
rule (`core/src/evaluator/capture.rs`):

- run `M` with the output captured;
- strip the trailing newline;
- decode the bytes as strict UTF-8 — `| from-bytes` keeps bytes that are not valid UTF-8.

`capture M` is close to the legal decoder tail `M | from-string`, which a user
can write. A decoder may end a byte pipeline; a value produced by that decoder
is then composed by application or bind, not by another pipeline edge.

**The calculus is a graded call-by-push-value.** The spec is the grading; `F`
remains a functor from value types to computation types, and the adjunction
with `U` is unchanged. Three properties hold:

- the result mode of a computation is stable under substitution and under abstraction;
- elaboration is total and type-preserving;
- coherence follows from one subsumption instance and one coercion.

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
