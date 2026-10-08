---
verified_at_commit: 0055ee6f
verified_at_date: 2026-10-01
anchors: [Inferencer, Idx, settle_index, settle_pending_indexes, Unifier, Kind, Kinded, admit, unite_tys, settle_by_kind, Pairs, bind_ty, bind_comp_ty, unify_row, infer_assembly_record, CachedFreeVars, WeakVars, mark_weak, reseed_weak, settle_weak, infer_record_val, infer_map_val, infer_label_read, settle_label, settle_pending_labels, generalize, instantiate, annotate, SessionSchemes, extract_return, force_return_shape, stage_root_stdin_feed, InferCtx, head_class, stands_in, join_arms, adapt, coerce, unify_arm, stage_writes]
---

# Type inference: the algorithm

[[design/types|The type system]] states *what* is well-typed; this is *how*
`core/src/typecheck/` infers it — constraint-based Hindley–Milner over the CBPV
[[map/core/ir|IR]] after [[internals/compilation-ladder|elaboration]].

**The Inferencer walks the typed IR.** `infer.rs` traverses `Val` / `Comp`,
allocating fresh type, computation and row variables and emitting unification
constraints as it goes; `infer_case` is kept whole at ~100 lines by decision
([[decisions/260530_infer-case-stays-whole|infer-case-stays-whole]]). Builtin
signatures enter through per-builtin rules carried with the body
([[internals/builtins-registry|builtins registry]]: `fixed_arity`,
`builtin_type_hint`), the one source of a table entry's arity
([[invariants/fixed-arity|fixed-arity]]). The base-frame manifest's rows have no
arity at all: their schemes are seeded into the checker's env at boot, so a base
frame is looked up as a handler is
([[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).

**A name is known statically, or it is refused.** A `$name` resolves against the
lexical and session bindings, then the value builtins, then the language's own
`true` and `false` (`LANGUAGE_CONSTANTS`, typed `Bool`); a miss is
`UnboundVariable` (T0071), never a fresh variable, and its hint ranks the nearest
names in scope by edit distance (`text::near_names`, shared with the record-key
decoder). The runtime's "undefined variable" survives only as a defensive
error. The same rule holds at a head: a binding met as a bare head is applied at
its thunk's computation type, a binding of still-unknown type is taken to be a
thunk, and a binding of anything else is `HeadBoundToValue` (T0072) —
`instantiate_comp` never mints a computation variable for it.

**A block argument is checked last, and against its own call.** `apply_args_capped`
unifies the arrow spine from the arguments in source order, but a `Val::Thunk`
argument contributes only `Thunk(α)` on that pass; its body is inferred after
the loop, against the `α` the spine has since ground. `check_comp` then pushes
that expectation inwards: a `Lam` met with a known `Fun` binds the parameter to
the type the expectation names instead of a fresh variable. Synthesis in source
order would check the body of `map { |x| … } $xs` while the element type was
still free, which is a difference the body can *observe* — the computed-key
index rule once observed it, and no longer does: `map { |x| $x[$x] } [1]` and the
same lambda extracted into a `let` are both refused, because a computed index
is decided by the store at the unit's end, not by what was known when its body
was read. Everything else has nothing to push inwards and is inferred as ever,
the caller unifying.

**The Unifier solves types and rows at once** (`unify.rs`):

- *Value and computation types* are equi-recursive, but **every cycle must cross
  data**: binding a variable to a structure that reaches it along `U`, `→`, `F`
  and `Handle` alone is refused as `CyclicType` (T0073) by a search at the two
  bind sites (`bind_ty`, `bind_comp_ty`), which stops at a list, map, record or
  variant ([[decisions/260930_recursion-is-guarded-by-data|recursion-is-guarded-by-data]]).
  A cyclic scheme prints with a μ. Termination of unifying the cycles that remain rests on
  a co-inductive guard (`Pairs`): re-entering an in-progress equality obligation
  is an immediate success — the cyclic fixed point. The guard memoizes symmetric
  {ty, comp}-var *root pairs* **and** *one-sided* obligations — a var root against
  a finite structural key of the other side — so the same equi-recursive type
  anchored at a ty-var on one side and a comp-var on the other still converges
  rather than overflowing the stack
  ([[decisions/260606_unify-one-sided-obligations|unify-one-sided-obligations]]).
  `row_key` fingerprints each slot's type. The depth of a key is a resource
  bound no key closes (below).- *Rows* unify by the Rémy rewrite (`unify_row`): an occurs check guards the
  tail variable, then mismatched labels are permuted past one another into a
  shared fresh tail ([[design/row-types|row-types]]). The A field met against
  `Empty` is an error: `Empty` says every label off the spine is absent, so the
  record on the `Extend` side has a label the other lacks, and the arm that
  matched the pair owns the message, being the only frame that knows the label.
  A label on both sides unifies its two types.

**A literal's kind is syntax, and a key's form picks the projection rule.**
The parser classifies a bracketed literal, so the checker has one rule per IR
constructor — `infer_record_val` and `infer_map_val`: a record's fields each
keep their own slot in a row, a map's values share one element type, and the
two types unify only with their own kind
([[design/records-and-maps|records-and-maps]]). A record literal with a spread is an
**update** of one base (`infer_assembly_record`): it demands of the base a slot
at each written label, at a fresh payload with nothing imposed on it, and
returns those labels at the types written over the base's own tail. So a field
may change its type, a spread never adds a label, and the result is flat. Indexing splits the same way,
on the key rather than the target: a `Val::String` key is
`infer_label_read`, which emits a deferred `Lbl(c, label, e)`, meaning
`c = [label: e | ρ] ∨ c = Map e` (`typecheck/index.rs`). `settle_label` fires
the disjunct the target's head selects, at creation and at every drain; a head
is a monotone fact of the store, so the verdict does not depend on statement
order. A label whose target is still a variable is closed as a record where
the generalising `let` owns that variable — `settle_pending_labels` computes
which, and hands `generalize` the `tied` variables of the labels it keeps — and
at the unit's end; nothing pending is refused. Any other key is a computed index,
`Idx(c, k, e)` — `c = [e] ∧ k = Int ∨ c = Map e ∧ k = String` — beside `Lbl` in
the same file. `settle_index` fires a disjunct at creation when the target's
head, the key's head, or either kind refutes the other; an integer literal key
is the list rule and never waits, which is why `{ |xs| $xs[0] }` stays generic.
The three operands are made weak when the `Idx` is made, whether or not it
settles, and settlement never lifts the mark, so which variables a `let`
quantifies is a fact about the syntax of its body and no verdict depends on the
order of independent statements. Nothing settles at a generalisation;
`settle_pending_indexes` runs at the unit's end, drains labels and indexes
together to a fixpoint, and refuses whatever index is still pending
(`IndexContainerUnknown`, T0075, with a second caret on the binding that holds
it, `InferCtx.holder`).

**Capture is decided by the type.** `CompTy::Return(Grade, Ty)` carries a grade
— `Value`, `Output` or a variable — in a fourth atomic union-find beside the
type, computation and row stores, quantified by `generalize` (`Scheme.grade_vars`)
and never weak. A *command* — an external, `^name`, a path head, a value row that
writes, or a handler arm standing in for one — is `Return(Output, Unit)`,
printed `Command`; `CompTy::command()` builds it and `CompTy::pure` is
`Return(Value, _)`. `unify` unifies the grades, then the values; a grade
disagreement is a `CompTyMismatch` reading `Command` against `Returns A`. The
invariant `F^w A ⇒ A = Unit` is rule-enforced and asserted in `generalize` and
the annotate walk ([[decisions/260930_graded-f|graded-f]]).

- `head_class` (`infer.rs`, beside `exec_comp_ty`) resolves a bare head by the
  order checker and runtime share: `Binding`, `Value(entry)`, `Arm`, `External`.
- `rhs_bound_ty`, shared by `Bind` and `Define`, is the bind rule: a `Fun` right-hand
  side binds a thunk; `Return(Output, _)` inserts the node's address in
  `InferCtx.captured` and binds `String`; `Return(Value, a)` binds `a`;
  `Return(Var(g), a)` binds `g := Value` and `a`; a computation variable opens
  into `Return(Value, fresh)`. `force_return_shape` opens a variable into a fresh
  *grade*, never `Value`: only the bind rule decides `p`.
- `apply_args_capped` coerces an argument ending in `Return(Output, Unit)` where
  the callee's parameter ends in `Return(Value, β)` at the same arity (`spine`
  reads the parameters and the `Producer` past them), recording a literal
  block's body address in `captured` when the block writes every lambda of
  that arity, else the block itself in `InferCtx.captured_vals` with its
  arity, as a value in hand.
- `discharge` applies to the `Exec` and `Redirect` rules when stdout is
  redirected: `Return(_, a)` becomes `Return(Value, a)`, through `Fun` results.
- `annotate` is a structural rebuild (the methods of `Annotate { ctx, eta }`) that
  wraps each recorded node in `cap M to d. decode d` (`CompKind::Capture` and
  `CompKind::Decode`, both checker-inserted); a value in hand becomes a lambda
  whose body captures the forced call.

So `let x = f` with `f = { hostname }`, `let x = !$t`, `let x = g 5` and
`let x = map { |f| echo $f } xs` all capture: β and η preserve meaning.

**Arms join by two passes.** Every form that suspends a command — `if`,
`case`, `try`, `?` — takes a thunk `U C` and forces the one it chooses;
`CompKind::If` and `CaseArm` carry a `Spanned<Val>`. `join_arms` infers every arm
against `Fun(params…, ρᵢ)` with a fresh result (`check_arm` pushing the
parameter types inward as before), resolves each `ρᵢ`, and decides the target:
`F^p τ` if some arm is `F^p A` with `A` not `Unit` or unresolved, capturing every
`Output` arm; else `F^w Unit` if some arm is `Output`, upcasting the `F^p Unit`
arms and binding grade variables to `w`; else unify. `infer_case` and
`infer_try` route through it. A disagreement is `T0010` or `T0020` between two
values, and `T0011` between shapes or grades; its hint is derived from the two
types (one arm is a command, whose value is its output). An `else`-less `if`
wraps its lone arm as `{ body; () }`. The join decides in program order with no
pending constraint, so `{ |t| if c { echo hi } else { !$t }; let w = !$t; $w }`
types `t : {Command}` while the same block with the `let` first types
`t : {Returns String}`.

**One shape rule, `force_return_shape`, does the work an adjacency rule used to.**
`extract_return` (`infer.rs`) resolves a `CompTy` to its return type, unifying
against a freshly minted `Return` (fresh grade and type) when it is still a variable. `infer_pipeline`
forces **every** stage through it under `Reason::PipelineStageShape`: a stage
typed `Fun` is a function still waiting for an argument, and the diagnostic says
to apply it rather than pipe into it. A stage feeds the next by writing, so
`stage_writes` unifies every stage but the last with `F^ε Unit` at its own grade
(any grade is accepted) and refuses a decoder by its own mark (`stage_decoder`,
`DecoderMidPipeline`, T0078). The pipeline's type is the last stage's; its value
is its final stage's, and the stage types are recorded for the structural REPL
along the way. The rule's one other premise reads no type at all:
`stage_root_stdin_feed` looks at a non-first stage's root redirects, and a `< f`
or `<< w` binding fd 0 there is `DeadPipeEdge` (T0070) — the feed answers the
stage's reads for its whole run, so the wire's producer writes for nobody
([[design/pipelines|pipelines]]).

**An arm stands in for the head it replaces.** `Inferencer::stands_in(name,
arm)` checks a handler or alias arm against `standsFor(name)`: the scheme of the
base frame or arm already in force under `name`, else `[String] → Command`. An
arm for `curl` therefore has a command's value, its output (a `()`-returning stub
is admitted by the upcast and captures `""`), and one for `detach` returns
what `detach` returns; the installed scheme has the head's producer kind. A refusal is `Reason::StandsIn(Standing)`, with
`Standing::{Command, Own, EveryCommand}` for a command, a head's own scheme and
the catch-all handler. The run-time install doors apply the same check
(`HandlerEntry::vet` through `alias_arm_scheme`, and the catch-all through
`catch_all_stands_in`) and render the same sentence (`types::refused_arm`).

**The `()` note.** `TyEnv` records, for a name `let` bound to what a call
returned (`note_called`, `called_head`), the callee; a read of that name at type
`()` is noted in `InferCtx.unit_reads`, and an error raised at that span whose
kind concerns `()` gains a sentence that `deploy` returns `()` and that
`| from-line` captures what it prints (`TypeError.unit`, `UnitCall`); the pipe
takes everything a stage writes, so the remedy is operationally right. `()` is
neither a word nor an interpolant: its kind is `scalar`.

**Principality is the textbook result, with its limits stated.** One rule reads the
order of statements: the join, which resolves its arms as they are met and holds
no pending constraint, so a grade it could have learned later is not waited for
(the `t : {Command}` against `{Returns String}` case above). Where a `let` captures
is decided by the type at three syntax-directed sites, and every other rule is
unification, so typing verdicts do not otherwise depend on the order the solver emits
constraints. The deferred constraints that remain (`Lbl`, `Idx`) are settled by
monotone facts of the store and drained at the unit's end. Shape verdicts — the
pipeline stage forcing, the sequence tail — are introduction-rule choices, not joins.**The row algebra is textbook.** A slot is a label and a type, so the Rémy
rewrite is unitary and principality over rows is the standard Hindley–Milner
result. The two rules that build a row from a program — the closed literal and
the update — mint every payload fresh or as written, and nothing else writes
into a row.

No declared table reaches the unifier. A form's options are consulted key by
written key at the form, and a contract file's return is ascribed its table
after inference, in a scratch copy of the unifier, never entering the store
([[decisions/260930_a-table-never-enters-the-unifier|a-table-never-enters-the-unifier]]).
So two tables naming one label at two types cannot let the order of two
constraints decide a verdict.

**One obligation is carried rather than discharged, and it is not a theorem.**
*Recursion*: the **policy is chosen and the proof is owed**. The occurs check descends through payloads as well as along the spine.
The alternative — occurs along the spine only — would admit
`ρ = (x: Record(ρ) ; Empty)`, a cycle anchored at no type or
computation variable: `apply_row_inner` carries no row-root guard,
`cyclic_roots_in_ty` cannot find it, and `Scheme` has no `row_bindings` to
snapshot it into. So spine-only is not an edit to the occurs check but a third
recursive sort, and this tree takes the descending policy.

`MAX_UNIFY_DEPTH` is a **resource bound and not a termination result**. What
is guaranteed throughout is a graceful `TypeTooDeep` rather than a blown stack.

**Generalisation is at the binding boundary** (`generalize.rs`). At each `Bind`
the inferencer takes the type's free variables minus those still free in the
environment and closes over the difference into a `Scheme`, each quantifier
list sorted by variable id so a scheme — and the letters its `Display` prints it
under — is a function of the type alone; `instantiate` refreshes a scheme's
bound variables at each use. Generalisation walks the
type structurally and unbudgeted, where unification charges a depth ceiling:
the walks are linear in a type the source built one constructor per statement,
and only unification descends past what the parser saw
([[invariants/term-depth|term-depth]]). The order
follows the SCC
structure the elaborator found — a non-recursive group generalises at its binding
point, a mutually recursive group stays monomorphic until its fixed point — which
is what keeps generalisation sound. A type error aborts with a positioned
expected-vs-inferred message (`fmt.rs`), where a computation reads `Returns A`, `Command`, or `ν A` for a grade
variable: `{ echo hi }` is `{Command}` and `{ 'hi' }` is `{Returns String}`.

**Deep types are checked without a guard, and checking them is superlinear.**
The shape that reaches the structural walks without ever charging `deeper()`
nests one constructor per binding (`let x1 = [f: x0]`, `let x2 = [f: x1]`, and
so on). Under `ral --check`, release build, 8 MB stack:

| nesting depth | wall clock | result |
| --- | --- | --- |
| 20,000 | ~1s | exit 0 |
| 50,000 | 54s | exit 0 |
| 150,000 | 11min | exit 0 |

- *No overflow and no `TypeTooDeep`* at 293x `MAX_UNIFY_DEPTH`: the unify
  budget never fires on this shape, and the walks survive alone.
- *Cost is superlinear*: 3x the depth is 12x the time, consistent with
  generalisation at each binding walking a type grown one constructor per
  binding, Θ(N²) visits (inferred from the curve, not profiled). A 50,000-deep
  type needs a 50,000-line file, so it is recorded, not urgent.

**Weak variables are the one subtraction.** `generalize` quantifies every
variable of the type that is neither free in the environment nor *weak*. A
variable of any sort — type, computation or row — is weak once
`Unifier::mark_weak` has said so, and the mark is permanent for the unit:
`Store::unite` hands it to the survivor, and a variable of any sort
unified with a weak one, even one already fixed to a structure, becomes weak, so
a later mismatch is still seen; binding a weak variable to a structure makes
every variable of the structure weak, with the same source. A scheme or binding keeps a weak variable as itself
(`apply_ty_keeping_weak`) until its unit ends. A weak
variable is treated as free in the environment, so a `let` over it is
monomorphic in it, and the scheme lists it in `Scheme.weak` instead of
quantifying it; its `Display` prints it `_α` after the binders. A pending label
read whose target is weak is tied in the same way (`settle_pending_labels`).
There is no value restriction: a `let` of anything generalises
([[decisions/260930_a-let-generalises-what-is-not-weak|a-let-generalises-what-is-not-weak]]).
A computed index marks its three operands, and so does a boundary call its result
([[decisions/260930_a-boundary-is-checked-against-its-type|a-boundary-is-checked-against-its-type]]);
once the unit is solved, `annotate` freezes each recorded result as a `Site` on
the call.
`settle_weak` closes a scheme at its unit's end: each weak variable it mentions
is resolved as far as the unit fixed it, and a cycle through one is snapshotted
like any quantified cycle (`cyclic_bindings`), so `μα. Map α` survives the unit.

**A type variable carries a kind** (`kind.rs`): a set of admissible heads and a
deep bit, and a row variable a deep bit alone. The unconstrained `any` has every
head; five are named — `number` (`+ - * /`, unary `-`), `comparable` (`<`, `lt`,
`gt`, `sort-list`, `int`, `float`), `scalar` (each interpolation part), `sized`
(`length`, `is-empty`) and `data`, every head but a block and a handle, *deep*
(`==`, `equal`, `str`, the encoders). The unifier holds them beside the store, on
the free variable's root: `Kinded { kind, witness }`, the witness being the span
in force (`Unifier::at`, set by `InferCtx` before it unifies or instantiates)
when the kind last narrowed. `unite_tys` unites at the meet and refuses an empty
one, citing both witnesses; a meet with one nullary head binds the variable to
it. `admit` runs at `bind_ty` and checks the head, and a deep kind walks the
structure (`data_ty`, `data_row`, with a visited set for a data cycle) and
imposes `data` on each free variable and deep bit it reaches; a row variable that
is deep imposes the same on whatever it is later bound to (`admit_row`). A
refusal is `KindMismatch` (T0074), which `explain.rs` words by the kind and
guides by the reason, and which `diagnostic.rs` draws with a second caret at the
witness. A kinded variable still free at the end of a unit needs no default.
Operators take a fresh kinded variable and unify both operands with it
(`infer_binary`), and so does each interpolation part; `%` is `Int → Int → Int`.
A scheme quantifies `(TyVar, Kind)` and `(RowVar, bool)` and prints `∀α:number.`;
instantiation re-mints each, and a weak residual keeps its kind
([[decisions/260930_operators-are-kinded|operators-are-kinded]]).

A bare-label read is settled by a head, or by a kind that admits `Map` and not
`Record` (`sized`), or `Record` and not `Map`, or neither, which is refused at the
read naming what else the variable was used as (`settle_by_kind`).

**The quantifier is the prefix, not a binder.** A `Scheme` is the body's
ordinary `Ty` under a ∀-prefix of three `Vec`s of variable ids — value
(`ty_vars`), computation (`comp_ty_vars`) and row (`row_vars`) sorts, each a
`u32`-tagged unifier root (`scheme.rs`). There is
no binder node and no de Bruijn index: a variable is
**bound iff it is listed**.

- *Elimination is substitution-with-freshening.* `instantiate` mints a fresh id
  in the current unifier for every listed variable and substitutes it through
  the body, so capture is impossible by construction — the fresh ids did not
  exist before the call.
- *Recursive types are not syntax but μ-equations attached to the prefix.*
  `comp_ty_bindings` / `ty_bindings` carry `(root, applied-binding)` pairs for
  each cycle in the body; `instantiate` re-ties them in fresh union-find slots,
  so two uses of one recursive scheme never share a cycle root.
- Because the prefix is nominal-by-listing, an open scheme leaving its minting
  unifier aliases another's variables: see [[invariants/schemes-leave-closed|schemes-leave-closed]].
- *The residual cache is checked across every sort.* `CachedFreeVars` holds
  the free variables a scheme did **not** quantify, and `env_free_vars` trusts
  it rather than re-walking. That holds by construction: residuals come from
  monomorphic environment bindings that outlive every scheme mentioning them, so
  no later step moves or binds one, and a standing `debug_assert` checks it.
**The verdict survives into the next run.** The checker is a transformation:
on success `annotate` writes each top-level name-bind's generalised `Scheme` onto
its `Bind` node — and each `Pipeline`'s per-stage value
types onto the node — the scheme closed against the empty environment so it
carries no residual variable that could alias the next run's fresh ids. The next
run's check is seeded from the live session — one `SessionSchemes` (the scope's
name→scheme map plus the alias arms' schemes) — so a name bound in run *N* is
checked at its inferred scheme in run *N+1*; a name from an unchecked path (a
`source`d file, a plugin) is `None` and infers afresh
([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).

A residual *weak* variable is the exception to that closure, and it is kept
across units rather than quantified: the scheme records it (`Scheme.weak`), a
unit settles its `Define` schemes before they are stored (`settle_weak`, so a
residual the unit fixed is stored as what it was fixed to), and `seed_env`
re-seeds each residual of a stored scheme as a fresh weak variable of the next
unit's unifier (`reseed_weak`). The name is then used at one type in the next
unit, as it was in the defining one, and no foreign id is ever read.

See also [[design/types|types]]; map [[map/core/typecheck|typecheck]]. Judgments:
`docs/SPEC.md` §17.3.
