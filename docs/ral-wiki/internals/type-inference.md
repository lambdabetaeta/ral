---
verified_at_commit: 1e9fea4
verified_at_date: 2026-08-06
anchors: [Inferencer, Unifier, Pairs, unify_row, unify_mode, generalize, instantiate, annotate, SessionSchemes, PipeMode, PipeSpec, extract_return, InferCtx, join_modes, alt_modes, join_arm_results, solve_at_boundary, solve_and_finalize, ModeConstraint]
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
  (`join_arm_results`), deferred as a stored constraint the moment the arms
  can't yet decide it; `InferCtx::solve_at_boundary` revisits it at the
  scheme boundary that owns its variables.
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

**A declared signature slot is still the only place a result-mode variable is
quantified.** Two kinds of slot carry one:

- a builtin's computation-typed argument, for example the callback passed to
  `spawn`, `each`, `map`, or `fold`;
- a scope's expected arm shape.

Each such variable is quantified in its `Scheme` beside the other mode
variables; `instantiate` refreshes it at each use like any other variable, a
call site's actual argument grounds it, and otherwise it stays quantified
inside an arrow that nothing consults.

A deferred `join_arm_results` does mint a result-mode variable of its own —
the target it hands back while the arms are undecided — so the restriction is
stated over lifetime rather than origin: **no result-mode variable outlives
its owning drain except as an alias of a declared slot variable or of a
ground mode.** The target is either concluded to a ground mode or collapsed
onto the arms' own variables; nothing downstream sees an unattached one.

**Principality holds for mode constraints, with its limits stated.** Typing
verdicts do not depend on the order the solver emits, retries, or collapses
constraints: every join — `join_modes` and `alt_modes` included — is a
constraint stored and re-examined at the scheme boundary that owns its
variables rather than a decision read off a partially-solved store;
conclusions are monotone on a height-1 lattice, ground-directed collapses are
serialised through the worklist, and the all-open residue equates by pure
union-find merges. Two limits are part of the claim: still-open ends joined
under one binding equate at the boundary that owns them (the completeness
frontier priced below), and two still-open `join_arm_results` sharing value
*type* variables at one boundary could in principle observe each other's
value unifications in collapse order — a corner no source program has been
made to reach. Shape verdicts — `consumes_value_arg`, the sequence tail —
sit outside the claim; they are introduction-rule choices, not joins.

**Three named join operations live in `core/src/typecheck/mode_solver.rs`,**
the only module permitted to case on a mode's groundness. Equality stays
`Unifier::unify_mode`, above; beside it stand:

- `join_modes` (`⊔`, bytes-dominant) — a form's channel end is the least
  upper bound of its parts': a `Seq`'s channel over its statements, a
  scope's over its arms. `∅` is the identity and `Bytes` is absorbing, and
  the join constrains the *target* only — it never writes back into an end,
  so a statement's still-open mode is never pinned by the sequence around
  it. Which ends a `Seq` even contributes is `lift_channels`' shape verdict,
  not a join: a `Fun` tail keeps the sequence a function, a `Return` tail
  joins its ends with the statements', and a still-unknown tail is forced
  into stage shape only when some statement's end has settled `Bytes` —
  otherwise the sequence is exactly its tail, free to become a function at
  its call site. An opaque statement (`force $t`) contributes fresh
  unattached ends, not `t`'s own: attaching them would force `t` into stage
  shape and reject a lambda argument.
- `alt_modes` — arms of which only one runs, so a clash is an unknown for a
  downstream stage to pin rather than a contradiction: ground and equal ends
  agree, ground and disagreeing ends leave the target free.
- `join_arm_results` — the result-mode join at the heart of every arm merge
  (reached from `merge_branches` for `if`, a `?` fallback chain, and `case`,
  and from `infer_try`), under the one subsumption instance
  `∅@Unit ⊑ Bytes@Unit`. An arm whose computation type is still a bare
  variable — a call to a function under inference, as in a recursive branch
  — is first forced to `Return` shape by `extract_return`, so it joins with
  the other arms instead of strict-unifying against them. A join with any
  byte-payload arm lands wholly on the byte side and ties every arm's value
  to `Unit`; with no byte-payload arm, a ground `None` arm at `Unit` is the
  identity, still-open arms unify with one another and stay open, and only
  an arm with a value payload (`None` at non-`Unit`) pins them to `None`.

A byte-payload arm alongside a ground `None` arm at a non-`Unit` type is a
type error. The two arms disagree about which conduit carries the payload.
The fix is an explicit pipe, for example `echo hi | from-string`, and not an
inserted coercion.

**Conclude, store, solve-what-you-own.** Each call above applies whatever
conclusion is already determined and defers the rest as a stored
`ModeConstraint` — applying early is sound because a mode only ever moves
`Var → ground`, never back, so an early conclusion can't be invalidated
later. `InferCtx::solve_at_boundary(env)` runs at every in-inference point
that produces a scheme — the `Bind` let-generalisation, `infer_letrec`'s
group fixpoint, `handler_comp_scheme` — and solves exactly the constraints
touching a mode variable not free in `env`: the variables about to be
quantified, which a constraint must not outlive. A constraint whose every
writable variable is still free in the environment belongs to an enclosing
binding and is left entirely untouched — not retried either, since a
conclusion's side effects pin arms still under inference elsewhere, so
running it at a boundary a syntactic accident placed (any inner `let`, the
elaborator's hoisted binds included) would foreclose a sibling's `∅@Unit`
subsumption or move the join's error onto the group unification.
`InferCtx::solve_and_finalize` is the terminal drain — end of check before
`annotate`, plus the empty-environment `alias_arm_scheme` and
`binding_value_scheme` — where everything collapses. Each drain propagates to
quiescence, since one conclusion can determine a sibling, then collapses what
it owns. **Collapse is directed by the target, then equates; it never
defaults**: a `join_modes`/`alt_modes` residue whose target grounded `Bytes`
from outside is satisfied and drops without touching its ends, a `∅` target
pins the open ends `∅`, and a `join_arm_results` whose result grounded lands
on that side with the side's full protocol. These ground-writing collapses
run one at a time with the worklist re-run between; only the all-open residue
then equates — open ends with each other and with the target — and a
collapsed variable can still ground `Bytes` afterwards, with the target
riding along. Defaulting stays exactly where it always lived, in
`InferCtx::ground`, at annotation time.

**The price of deferral:** two open ends joined under one binding equate at
the collapse of the boundary that owns them, so mode polymorphism holds *up
to joined ends sharing a variable* —
`{|t,u| if $c { force $t } else { force $u }}` comes out with one shared `μ`
rather than two independent ones. See
[[decisions/260807_modes-solved-by-deferred-joins|modes-solved-by-deferred-joins]]
for the full account of what the solver replaced and why.

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
