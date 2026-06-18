---
verified_at_commit: 7ba500b
verified_at_date: 2026-06-17
anchors: [Inferencer, Unifier, Pairs, unify_row, unify_mode, generalize, instantiate, annotate, SessionSchemes, PipeMode, PipeSpec]
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
- *Byte modes* unify too, *by equality* (`unify_mode`): the `∅` / `Bytes` / `μ`
  channel modes (`PipeMode`) on `F[I,O] A` are first-class unification variables,
  but a ground `∅` and a ground `Bytes` do not unify — a value edge cannot meet a
  byte edge. The lattice (`PipeMode`, `PipeSpec`) lives in `core/src/mode.rs` and
  the equality rule is `Unifier::unify_mode` — a plain method now that the static
  checker is the sole mode engine
  ([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]) — so a
  pipeline edge mismatch is a live `ModeMismatch` (T0012) at type-check time rather
  than a silent coercion ([[design/pipelines|pipelines]];
  [[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]]).

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

**The verdict survives into the next turn.** The checker is a transformation:
on success `annotate` writes each top-level name-bind's generalised `Scheme` onto
its `Bind` node — and the ground mode `Wire` onto every `Pipeline` stage and `Bind`
RHS ([[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]) — the
scheme closed against the empty environment so it carries no residual variable that
could alias the next turn's fresh ids. The next turn's
check is seeded from the live session — one `SessionSchemes` (the scope's
name→scheme map plus the alias arms' schemes) — so a name bound in turn *N* is
checked at its inferred scheme in turn *N+1*; a name from an unchecked path (a
`source`d file, a plugin) is `None` and infers afresh
([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).

See also [[design/types|types]]; map [[map/core/typecheck|typecheck]]. Judgments:
`docs/SPEC.md` §20.
