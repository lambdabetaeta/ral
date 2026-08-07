---
status: active
---

# Modes get a solver: joins are deferred, and a boundary solves only what it owns

The type checker is graded CBPV. The CBPV half — equi-recursive unification,
rows by Rémy rewrite, cycle-snapshotting schemes — has been quiet since June.
The grading half has not: six fixes in ten weeks (`852be2ac`, `14654474`,
`c3f0bb0b`, `27e84d3f`, `9cd86640`, `9c5c8d3c`), every one of them in the same
fragment, the mode joins over branch arms, sequences, and scopes.

## What was wrong

The cause was structural, not a run of careless patches.

The mode lattice `∅ ⊑ Bytes` semantically requires a **join**. The solver only
had **equality** (`Unifier::unify_mode`). So every join site improvised the
join in its own way, by resolving the unifier's *current* state and casing on
what happened to be ground: `join_byte_mode`, `union_mode`,
`join_arm_results`'s open-arm protocol, `merge_branches`' all-or-nothing fold,
`lift_modes`' two `bool` flags. Each improvisation is an eager decision taken
against a partially-solved store, and is therefore order-sensitive by
construction — the answer depends on when the checker happened to visit the
constraint relative to when some arm's mode got grounded.

The characteristic victim is an arm whose mode is its own binding group's,
still under inference: a recursive call in a branch. Read too early it looks
silent, and the decision taken against that reading is never revisited.
`852be2ac` ("still-open arms stay open and ground at the binding group's fixed
point") is lazy constraint solving, hand-implemented for one mode, in one
construct family.

The wiki's principality claim — "no typing decision depends on the order the
solver visits constraints" — was proven only for *result*-mode variables, via
the slot restriction. The input and output joins had no such property and no
one had claimed otherwise.

## What we decided

One constraint store, in `core/src/typecheck/mode_solver.rs`, owning the
**only** logic in the checker that computes a join by casing on a mode's
groundness. Four reads stay outside as *shape verdicts*, not joins —
`infer_pipeline`'s byte-tail decision, `Bind`'s result pin,
`consumes_value_arg`, and `lift_channels`' tail-shape verdict (below) — each
inspecting settled state to apply its own introduction rule.

**Three operations, named apart.** Equality stays equality — pipeline
adjacency is genuine unification, ground `∅` never meets ground `Bytes`
(T0012), and `unify_mode` is untouched. Beside it now stand two others:

- **`Join`** (`⊔`, bytes-dominant) — a form's channel end is the least upper
  bound of its parts' ends: a `Seq`'s channels over its statements, a scope's
  over its arms, a branch's over its arms. `∅` is the identity, `Bytes` is
  absorbing, and the join constrains the **target only** — it never writes
  back into an end.
- **`Alt`** — arms of which only one runs. Ground and equal ends agree; ground
  and disagreeing ends leave the target free, because a clash between arms
  that never both run is an unknown a downstream stage can pin, not a
  contradiction.
- **`ArmResults`** — the arms of an `if`/`?`/`case`/`try` agreeing on which
  conduit carries the payload, under the one subsumption instance
  `∅@Unit ⊑ Bytes@Unit`. Unlike the channel join, this one *does* discipline
  the arms: it pins open results and ties byte-side values to `Unit`.

**Conclude, store, solve-what-you-own.** Emission applies whatever conclusion
is already determined and stores the rest. Applying early is sound because a
mode only ever moves `Var → ground`, never back, so an early conclusion cannot
be invalidated. From there the store drains at two kinds of point.
`solve_at_boundary(env)` runs at every in-inference point that produces a
scheme — the `Bind` let-generalisation, `infer_letrec`'s group fixpoint,
`handler_comp_scheme` — and solves exactly the constraints touching a mode
variable not free in `env`: the variables `generalize` is about to quantify,
and quantification is what a constraint must not outlive. A constraint whose
every writable variable is still free in the environment belongs to an
enclosing binding and is left **entirely untouched** — not collapsed, and not
retried either. The bookkeeping is the generalisation criterion itself,
computed by `env_free_vars` only when the store is non-empty. It has to gate
retry as well as collapse: a conclusion's target is order-invariant, but the
byte side's side effects pin arms still under inference elsewhere, so a
conclusion run at a boundary a syntactic accident placed — any inner `let`,
the elaborator's hoisted binds included — can foreclose a sibling's `∅@Unit`
subsumption and reject a program the owning boundary accepts, or move the
join's error onto the group unification. `solve_and_finalize` is the terminal
drain — end of check before `annotate`, and the empty-environment scheme
builders `alias_arm_scheme` and `binding_value_scheme` — where nothing
encloses the store and everything collapses. Propagation to quiescence is a
worklist over a height-1 lattice, trivially terminating.

**Collapse is directed by the target, then equates; it never defaults.** A
residual constraint whose target a neighbour grounded from outside is settled
by that target, not equated through it. A `Join`/`Alt` whose target reads
`Bytes` is *satisfied* and drops — a form's end may exceed its parts' use, and
the join never writes back into an end; a `∅` target pins the open ends `∅`,
the one forced direction; an `ArmResults` whose result grounded `Bytes` or `∅`
lands on that side with the side's full protocol — byte-side value-to-`Unit`
tying included, which a bare equation would skip. These ground-writing
collapses (every `ArmResults` among them, since its value unifications ground
types whichever side it lands) run one constraint at a time with the worklist
re-run between, so a write that determines a sibling reaches the sibling's own
rule instead of being raced by an equation. Only then does the all-open
residue equate — open ends with each other and with the target, pure
union-find merges no order can observe. A collapsed variable can still ground
`Bytes` later, and the target rides it. Defaulting stays exactly where it
already lived, in `InferCtx::ground`, at annotation time.

This keeps the invariant that keeps schemes simple, stated over ownership:
**no constraint outlives the generalisation of its variables.** Schemes
quantify plain mode variables exactly as before, nothing new serialises, and
[[decisions/260603_session-scheme-continuity|session-scheme-continuity]] is
untouched. The ownership criterion counts writable positions only —
`Join`/`Alt` targets and ends, `ArmResults` results and outputs, never an
arm's input, which no rule writes — and modes only: a kept constraint whose
*value types* mention a locally quantified type variable would let an inner
scheme's instantiations escape the value agreement, a corner reachable only
through contrived nesting and bounded by the owning boundary's collapse.

**The sequence tail is a shape verdict, not a join.** A `Seq`'s statements
run for their effect; the tail gives the sequence its value and may be a
function. `lift_channels` keeps a `Fun` tail untouched, joins a `Return`
tail's ends with the statements', and forces a still-unknown tail into stage
shape only when some statement's end has *settled* `Bytes` — a demand needs a
spec to live on — leaving it otherwise free to resolve `Fun` at its call site
(`{ |f| return unit; !$f }` applied to a lambda). This is a state-inspection
beside `consumes_value_arg`, with the laxity that entails, stated: a
statement whose byte demand settles only after the sequence closes does not
reach a still-free tail, and an opaque statement (`force $t`) contributes
fresh unattached ends rather than `t`'s own, since attaching them would force
`t` into stage shape and reject a lambda argument. HM offers no disjunction
over a tail's eventual shape; the alternatives — always forcing, which
rejects the higher-order idioms, or never forcing, which drops a settled
stdin demand on the floor, a golden-rule hole — are both worse.

## What it buys

Each of these is a bug the old shape had, or would have had again:

- An open arm that grounds `Bytes` before the boundary re-evaluates the whole
  join on the byte side with the subsumption check intact. Previously the
  no-bytes path had already unified the values and the check never re-fired.
- Two open sibling arms that ground *differently* before the boundary get the
  join's own error under the join's own provenance. Previously `union_mode`
  had already equated them, so the clash surfaced at whatever unrelated site
  touched the shared variable next — or silently mis-moded a silent sibling as
  byte-emitting.
- A `Seq` holding `force $t` with the statement's ends still open generalises
  with its channel open and quantified — a variable a later grounding can
  still move. The variable is the sequence's own, not `t`'s: an opaque
  statement contributes no attachment to `t`'s channels, because tying them
  down would force `t` into stage shape and reject a lambda argument.
  Previously `lift_modes`' eager `bool` read `false` and stamped the `Seq`
  silent for good.
- The byte side ties *every* arm's value to `Unit`, pinned-open arms included.
  Previously the byte path pinned a `Var` arm's result and never touched its
  value — a WF-2 gap the declarative reading closes for free.

**WF-1 and WF-2 move into the solver**, asserted and enforced per arm where an
`ArmResults` lands on the byte side, rather than debug-asserted at each
constructing site. `infer_pipeline` keeps its own two assertions — it decides a
byte tail by reading settled modes, not by joining — and `builtins.rs` keeps
its, which check hand-written signature tables at construction and have
nothing to do with joins.

**Principality, scoped and true.** For mode constraints, typing verdicts do
not depend on the order the solver emits, retries, or collapses constraints:
conclusions are monotone on a height-1 lattice, ground-directed collapses are
serialised through the worklist, the all-open residue is pure equation, and
ownership fixes *which* boundary equates. Two limits are part of the claim.
Still-open ends joined under one binding equate — monomorphise — at the
boundary that owns their variables: the deliberate incompleteness relative to
qualified schemes, priced below, tied to variable ownership rather than to
whichever inner `let` the elaborator happened to hoist. And two still-open
`ArmResults` sharing value *type* variables at one boundary could in
principle observe each other's value unifications in collapse order — a
corner no source program has been made to reach, left open because closing it
needs payload-deferral machinery out of proportion to it. The shape verdicts
listed above sit outside the claim entirely; they are introduction-rule
choices HM cannot defer, `consumes_value_arg` and the tail verdict foremost.

The slot restriction is restated rather than abandoned. A deferred
`ArmResults` does mint a fresh result-mode variable as its target, which the
old rule ("no source typing rule mints a free result-mode variable") forbade.
The honest form of the invariant is over lifetime: *no result-mode variable
outlives its owning drain except as an alias of a declared slot variable or
of a ground mode.* The target is either concluded to a ground mode or
collapsed onto the arms' own variables; nothing downstream ever sees an
unattached one.

## The price, stated

Two open ends joined under one binding equate at the collapse of the boundary
that owns them, so mode polymorphism holds *up to joined ends sharing a
variable*: `{|t,u| if $c { force $t } else { force $u }}` comes out with one
shared `μ` rather than two independent ones. This is the known completeness
frontier of the grading, not a regression — the old code equated them too,
only earlier and less predictably. A mixed constraint, one foot on an
enclosing binding's variable and one on a local one, is the frontier's edge
case: it collapses where the local variable generalises, monomorphising the
local end onto the enclosing one — the same equation qualified schemes would
avoid, taken at the innermost point that must take it.

## Alternatives rejected

- **Qualified schemes carrying mode constraints (HM(X)).** The principal fix
  for the shared-variable corner above: schemes would quantify constrained
  variables and constraints would survive generalisation. Rejected for now —
  it costs the "no constraint outlives generalisation" invariant, every
  serialised scheme, and session-scheme continuity, to buy a corner no real
  program has hit. Revisit only if one does.
- **`Payload = Value(Ty) | Bytes` inside the `Return` type**, making WF-2 true
  by construction. Rejected: it churns the IR, every scheme, and serde, for a
  representation win that does not remove the order-sensitivity — which is the
  actual disease.

## Deliberately not decided here

**Whether branch inputs should join.** `Alt` reproduces today's leniency: arms
disagreeing on stdin yield an unknown for a downstream stage to pin. The
alternative — inputs join too, so a form with any byte-reading arm demands a
byte upstream — closes a real hole: `if $c { from-json } else { return 5 }` fed
by a value edge passes statically today and hands `from-json` a channel that is
not there. It also tightens existing programs and churns goldens. That is its
own decision, to be taken in the vocabulary this one creates; the constraint
language supports either by swapping the emission.

Two neighbours stay queued behind as well: the checker's re-parsing of
`Exec("alias", …)` IR inside `infer_seq_with_alias_bindings`, which wants a
dedicated IR node rather than `alias_statement_shape`; and `infer_map_val`'s
`"plugins"` special case, already promised to the rc static-schema layer.
