# The checked IR is pure CBPV

**The checked IR has three layers: control, primitives on values, and shell.**
The core is read against `dev/agda/Extrinsic/`;
the evaluator is a CEK machine for [[design/cbpv|call-by-push-value]], and the
Agda machine its substitution-based CK counterpart. **Owned divergence D4:**
the acquiring `guard` and its ending-reading release await the strong-model
Agda follow-up ([[decisions/261111_a-guard-acquires|a-guard-acquires]]).
The correspondence excludes that rule until the follow-up re-establishes it.
Residual-function boxing also awaits an Agda follow-up: its Let-fun still
suspends the whole RHS, without Box. Runtime builtin η-expansion and δ
require their own correspondence follow-up
([[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]]).

- **Control:** `Return`, `Force`, `Lam`, `App`, `Bind`, `Seq`, `Rec`, `If`,
  `Case`, `Try`, `Guard`, `Capture`. A lambda binds one name;
  `Bind` takes a pattern, generalising its binders over one shared set of
  variables. State is a computation closure or `Err(Break)`, not a stored
  value. `settle` hands a produced value to frames in the same step; failure
  is not a computation constructor.
- **Primitives:** `CompKind::Prim` contains `Binary`, `Negate`, `Not`,
  `Render`, `Concat`, `Decode`, `Index`, `Update`, `List`, `Map`. Every
  operand is a value. A primitive computes on those values and may fail;
  it neither resolves a shell name nor runs another computation.
- **Shell:** `Exec`, `Pipeline`, `Redirect`, `Within`, `Grant`, `Spawn`,
  `Gather`, `Audit`, `Tilde`, `Use`. `Use` holds
  a linked checked module, closed under Σ; evaluation runs that tree and
  never resolves or reads a module file. Resolved full-arity
  sample builtins correspond to Agda's `prim (builtin b)`; the shell forms
  themselves are outside the core.

**Surface conveniences unfold before evaluation.**

- A parameter chain becomes nested name-binding lambdas, followed by binds
  for destructuring parameters. A later plain parameter also gets a bind if
  an earlier destructuring parameter binds its name, preserving shadowing.
- A written sequence is `Seq`; a written `let`, including `let _`, is `Bind`.
  Only these written continuations add the abandoned-steps hint on failure.
- Map operands are hoisted into binds in written order: key, then value,
  with each spread in place. `Prim::Map` validates and assembles after all
  operands have run; a repeated computed key fails then. Explicit entries
  beat spreads, and the first spread wins.
- Interpolation hoists every part's computations first, then renders the
  nonliteral parts in written order and concatenates all parts once.
- A value head is hoisted and run before its arguments, whatever its type:
  every `let` runs its right-hand side. A λ with no argument returns its
  occurrence-restricted closure as a thunk.
  Application has one `Arg` frame per argument; a λ consumes
  one. A builtin forces to `λa₀.…λaₙ₋₁.δ(b; a⃗)` over its captured entry;
  ordinary β binds its arguments. A partial builtin is the boxed residual λ.
  Runtime-only `CompKind::Builtin` is δ; the elaborator writes none;
  it reads the entry and arguments from ρ. An exact-arity builtin call invokes
  its body directly. Boundary partials remain inference-expanded over `Exec`,
  retaining their checked site.
  A spread belongs to argv, not value application: elaboration refuses it
  for written value heads and lexically bound heads;
  checking refuses it when a bare head resolves to a builtin or session binding.
- The checker writes returned-thunk application as `M to h. force h · V`;
  binding capture as `Capture(M) to d. Prim::Decode(d)`.
  No runtime rule forces implicitly or converts a value producer to a command.
- Boundary sites admit first-order data at the door; imported residual data
  is readmitted before the unit runs. A stored name whose scheme reaches a
  weak residual beneath a thunk or handle is statically unusable in a later
  unit, following μ-equations in both contexts. No returned value owes a
  delayed contract check, and the runtime builds no contract IR.
- `Val` is the one value syntax, including runtime `Bytes`, `Builtin` and
  `Handle` constants the elaborator never writes. `ValClos` closes a
  nonvariable `Val`, binding every direct name in ρ; formation restricts and
  pulls missing direct names from Σ, while component reads share ρ.
- A variant always has a syntax payload: a bare tag carries `()`.
  `View::Variant` omits any resolved unit payload, even one named by a variable.
- A form's options are one spread-free record node. `within`'s `handlers:`
  is an ordinary map-valued field; command names are data, with no
  handler-specific syntax, AST or IR node.

**A new IR form must be classified as control, primitive, or shell.** A core
form requires an Agda counterpart, including its typing and machine rule or
primitive signature and interpretation; the standing gaps above remain open.
The correspondence and its standing gaps are recorded in
[[decisions/261102_the-checked-ir-is-the-calculus|the checked IR is the calculus]]
and `dev/agda/README.md`.

See [[map/core/ir|IR]], [[internals/compilation-ladder|the compilation ladder]],
[[internals/evaluator-machine|the evaluator machine]],
[[invariants/fixed-arity|fixed arity]].
