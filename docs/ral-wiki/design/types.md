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

**Computation types carry byte modes.** A computation has type `F[I,O] A`: it
produces a value of type `A` while consuming input bytes `I` and emitting output
bytes `O` on the byte channel, where each mode is `∅`, `Bytes`, or a mode
variable `μ`. A parameterised block has type `{A → B}`.

- **The modes are *pipeline modes*** the inferencer infers and unifies
  (`PipeMode` / `ModeVar` in `core/src/mode.rs`): they make a stage's pipe
  behaviour visible to the checker, so adjacent stages connect only when the left
  output mode equals the right input mode.
- **This is where the value/command distinction becomes a *typing* discipline.**
  An external command has shape `F[I, Bytes] String` — bytes on the output
  channel, return type still `String`.
- **Decoding to that `String` is a *lossy*, hence total, UTF-8 conversion,**
  deferred to the `let` boundary rather than performed inside the pipe.

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

Cite: `docs/SPEC.md` §20;
RATIONALE §"Three layers, one asymmetry", §"External commands return strings".
