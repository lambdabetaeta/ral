---
verified_at_commit: 1e9fea4
verified_at_date: 2026-08-06
anchors: [Inferencer, Unifier, Pairs, unify_row, unify_mode, generalize, instantiate, annotate, SessionSchemes, PipeMode, PipeSpec, extract_return, join_arm_results]
---

# Type inference: the algorithm

[[design/types|The type system]] states *what* is well-typed; this is *how*
`core/src/typecheck/` infers it — constraint-based Hindley–Milner over the CBPV
[[map/core/ir|IR]] after [[internals/compilation-ladder|elaboration]].

**The Inferencer walks the typed IR.** `infer.rs` traverses `Val` / `Comp`,
allocating fresh type, row, and mode variables and emitting unification
constraints as it goes; `infer_case` is kept whole at ~100 lines by decision
([[decisions/260530_infer-case-stays-whole|infer-case-stays-whole]]). Builtin
signatures enter through per-builtin rules carried with the body
([[internals/builtins-registry|builtins registry]]: `builtin_arity`,
`builtin_type_hint`), the one source of arity
([[invariants/fixed-arity|fixed-arity]]).

**The Unifier solves three sorts at once** (`unify.rs`):

- *Value and computation types* are equi-recursive — unified with **no** occurs
  check, so cyclic types are admitted rather than rejected. Termination rests on
  a co-inductive guard (`Pairs`): re-entering an in-progress equality obligation
  is an immediate success — the cyclic fixed point. The guard memoizes symmetric
  {ty, comp}-var *root pairs* **and** *one-sided* obligations — a var root against
  a finite structural key of the other side — so the same equi-recursive type
  anchored at a ty-var on one side and a comp-var on the other still converges
  rather than overflowing the stack
  ([[decisions/260606_unify-one-sided-obligations|unify-one-sided-obligations]]).
- *Rows* unify by the Rémy rewrite (`unify_row`): a row-spine occurs check guards
  the tail variable, then mismatched labels are permuted past one another into a
  shared fresh tail — which is what makes scoped-label shadowing coherent without
  a restriction operator ([[design/row-types|row-types]]).
- *Modes* unify too, *by equality* (`unify_mode`): the `∅` / `Bytes` / `μ`
  modes (`PipeMode`) of a spec `⟨i, o, r⟩` are first-class unification variables,
  and all three ride the same rule, but a ground `∅` and a ground `Bytes` do not
  unify — a value edge cannot meet a byte edge. The lattice (`PipeMode`, `PipeSpec`) lives in `core/src/mode.rs` and
  the equality rule is `Unifier::unify_mode` — a plain method now that the static
  checker is the sole mode engine
  ([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]) — so a
  pipeline edge mismatch is a live `ModeMismatch` (T0012) at type-check time rather
  than a silent coercion ([[design/pipelines|pipelines]];
  [[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]]).

**The result mode names the payload's conduit** (`PipeSpec.result`,
`core/src/mode.rs`). Its value is `None` or `Bytes`, with no third case. Every
source-tree node grounds its own result mode:

- An introduction rule sets a ground result mode where it builds a node's type.
- Propagation copies a result mode from a sub-term.
- A join over branches computes a result mode from its arms
  (`join_arm_results`); a join whose only informative arms are still open
  stays open, and the enclosing binding group's fixed point grounds it.
- `extract_return`'s shape-forcing expectation grounds an otherwise-unresolved
  result mode.

A payload decision taken against an unresolved result mode pins it to `None`.

**Two well-formedness conditions hold wherever a rule builds a `Return` type.**

- WF-1: `result ⊑ output`. A payload on the byte channel needs a byte channel.
- WF-2: `result = Bytes` implies a `Unit` return type. A computation never
  carries both a value payload and a byte payload.

`Unifier::unify_mode` treats a ground result mode by the same equality rule as
a ground input or output mode. Two different ground results never unify. One
subsumption rule relates them instead, as a judgment and not as a unification
step. A computation of type `⟨i, o, None⟩ Unit` also has type
`⟨i, o, Bytes⟩ Unit` when `o` permits bytes. The rule fires only at the top of
a `Return` type. It needs no variance clause through `Thunk`, `Fun`, or a row.

**A result-mode variable exists only in a declared signature slot.** No source
typing rule mints a free result-mode variable. Two kinds of slot carry one:

- a builtin's computation-typed argument, for example the callback passed to
  `spawn`, `each`, `map`, or `fold`;
- a scope's expected arm shape.

Each such variable is quantified in its `Scheme` beside the other mode
variables. `instantiate` refreshes it at each use like any other variable. No
elaboration step reads a slot variable on its own. A call site's actual
argument grounds it. Otherwise it stays quantified inside an arrow that
nothing consults.

Because of this restriction, a tree node's principal type holds
unconditionally. No typing decision depends on the order the solver visits
constraints.

**A join over branches computes one result mode for all its arms**
(`join_arm_results`, reached from `merge_branches` for `if`, a `?` fallback
chain, and `case`, and from the `try` rule). An arm whose computation type is
still a bare variable — a call to a function under inference, as in a
recursive branch — is first forced to `Return` shape by `extract_return`, so
it joins with the other arms instead of strict-unifying against them.

- A join with any byte-payload arm lands wholly on the byte side.
- A ground `None` arm subsumes there only if its own return type is `Unit`.
- A still-unresolved arm's result mode pins to `Bytes`.
- With no byte-payload arm, a ground `None` arm at `Unit` is the join's
  identity — it decides nothing, because subsumption lets it ride either
  side. Still-open arms unify with one another and stay open; only an arm
  with a value payload (`None` at non-`Unit`) pins them to `None`. An open
  join grounds later, at its binding group's fixed point or at the first
  payload decision.

A byte-payload arm alongside a ground `None` arm at a non-`Unit` type is a
type error. The two arms disagree about which conduit carries the payload.
The fix is an explicit pipe, for example `echo hi | from-string`, and not an
inserted coercion.

**Generalisation is at the binding boundary** (`generalize.rs`). At each `Bind`
the inferencer takes the type's free variables minus those still free in the
environment and closes over the difference into a `Scheme`; `instantiate`
refreshes a scheme's bound variables at each use. The order follows the SCC
structure the elaborator found — a non-recursive group generalises at its binding
point, a mutually recursive group stays monomorphic until its fixed point — which
is what keeps generalisation sound. A type error aborts with a positioned
expected-vs-inferred message (`fmt.rs`).

**The quantifier is the prefix, not a binder.** A `Scheme` is the body's
ordinary `Ty` under a ∀-prefix of four `Vec`s of variable ids — value
(`ty_vars`), computation (`comp_ty_vars`), mode (`mode_vars`), row (`row_vars`)
sorts, each a `u32`-tagged unifier root (`scheme.rs`). There is no binder node
and no de Bruijn index: a variable is **bound iff it is listed**.

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

**The verdict survives into the next run.** The checker is a transformation:
on success `annotate` writes each top-level name-bind's generalised `Scheme` onto
its `Bind` node — and the ground mode `Wire` onto every `Pipeline` stage
([[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]) — the
scheme closed against the empty environment so it carries no residual variable that
could alias the next run's fresh ids. The next run's
check is seeded from the live session — one `SessionSchemes` (the scope's
name→scheme map plus the alias arms' schemes) — so a name bound in run *N* is
checked at its inferred scheme in run *N+1*; a name from an unchecked path (a
`source`d file, a plugin) is `None` and infers afresh
([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).

See also [[design/types|types]]; map [[map/core/typecheck|typecheck]]. Judgments:
`docs/SPEC.md` §20.
